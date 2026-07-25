use std::fs;

use unity_asset::AssetLoadBudget;
#[cfg(not(feature = "decode"))]
use unity_asset::extraction::ExtractionFilter;
use unity_asset::extraction::{
    ExistingOutputPolicy, ExtractionArtifactStatus, ExtractionDiagnosticCode,
    ExtractionExecutionError, ExtractionExecutionLimits, ExtractionExecutionOptions,
    ExtractionExecutor, ExtractionFailurePolicy, ExtractionManifest, ExtractionPlanError,
    ExtractionPlanner, ExtractionRepresentationPolicy, ExtractionRequest,
};
use unity_asset::reference::ReferenceGraphBuildOptions;
use unity_asset::workspace::{AssetWorkspace, WorkspaceLookup, WorkspaceView};

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
            workers.max(1),
            32 * 1024 * 1024,
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
            &options(1, ExistingOutputPolicy::Error),
            None,
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
            &options_with_failure(
                2,
                ExistingOutputPolicy::Error,
                ExtractionFailurePolicy::StopInPlanOrder,
            ),
            None,
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
            &options(1, ExistingOutputPolicy::Error),
            None,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let many = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &many_output,
            &options(4, ExistingOutputPolicy::Error),
            None,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let open_file_limited = ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::new(4, 8 * 1024 * 1024, 1, 32 * 1024 * 1024, 8 * 1024 * 1024)
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
            &open_file_limited,
            None,
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
            &options(2, ExistingOutputPolicy::Error),
            None,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let corrupted = &first.manifest().artifacts()[0];
    let corrupted_path = output.join(corrupted.path().as_str());
    let mut bytes = fs::read(&corrupted_path).unwrap();
    bytes[0] ^= 0xff;
    fs::write(&corrupted_path, &bytes).unwrap();

    let rejected = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            &options(2, ExistingOutputPolicy::Error),
            Some(first.manifest()),
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
            &options(2, ExistingOutputPolicy::Replace),
            Some(first.manifest()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(resumed.counts().written(), 1);
    assert_eq!(resumed.counts().resumed(), 1);
    let rebuilt = &resumed.manifest().artifacts()[0];
    assert_eq!(rebuilt.status(), ExtractionArtifactStatus::Written);
    assert_eq!(rebuilt.digest(), corrupted.digest());
    assert_eq!(
        fs::metadata(corrupted_path).unwrap().len(),
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
            &options(1, ExistingOutputPolicy::Error),
            None,
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
            &options(2, ExistingOutputPolicy::Skip),
            None,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(skipped.counts().skipped_existing(), 2);

    let rejected = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            &options(2, ExistingOutputPolicy::Error),
            None,
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
            &options(2, ExistingOutputPolicy::Replace),
            None,
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
        ExtractionExecutionLimits::new(2, 1, 2, 32 * 1024 * 1024, 8 * 1024 * 1024).unwrap(),
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
            &working_set_limited,
            None,
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ExtractionExecutionError::WorkingSetExceedsLimit { .. }
    ));
    assert!(!working_set_output.exists());

    let report_limited = ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::new(2, 8 * 1024 * 1024, 2, 32 * 1024 * 1024, 1).unwrap(),
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
            &report_limited,
            None,
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
        .execute_with_manifest(
            &snapshot,
            &plan,
            &output,
            manifest_path,
            &options(1, ExistingOutputPolicy::Error),
            None,
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
        ExtractionExecutionLimits::new(2, 8 * 1024 * 1024, 2, 1, 8 * 1024 * 1024).unwrap(),
        ExistingOutputPolicy::Error,
        ExtractionFailurePolicy::CollectAll,
    )
    .unwrap();

    let report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            &limited,
            None,
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
    workspace
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
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let new_snapshot = workspace.snapshot();
    let error = ExtractionExecutor::new()
        .execute(
            &new_snapshot,
            &plan,
            &output,
            &options(1, ExistingOutputPolicy::Error),
            None,
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
    let planner = ExtractionPlanner::new(&snapshot).with_reference_graph(&graph);
    let address = planner
        .bundle_container_addresses("*", &mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .next()
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
                "*".to_owned(),
                vec![address.clone()],
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
            &options(1, ExistingOutputPolicy::Error),
            None,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let handle_report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &handle_plan,
            &directory.path().join("handle"),
            &options(1, ExistingOutputPolicy::Error),
            None,
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
fn unsupported_binary_classes_are_reported_without_silent_raw_downgrade() {
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(sample("char_118_yuki.ab"), &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let planner = ExtractionPlanner::new(&snapshot);
    let preferred = planner
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::PreferDecoded),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
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

    assert_eq!(
        artifact.preferred_kind(),
        unity_asset::extraction::ExtractionArtifactKind::BinaryRaw
    );
    let output = tempfile::tempdir().unwrap();
    let report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &preferred,
            output.path(),
            &options(1, ExistingOutputPolicy::Error),
            None,
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
