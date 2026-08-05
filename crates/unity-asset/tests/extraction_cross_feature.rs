#![cfg(not(feature = "decode"))]

use std::fs;
use std::path::PathBuf;

use unity_asset::AssetLoadBudget;
use unity_asset::extraction::{
    ExistingOutputPolicy, ExtractionArtifactKind, ExtractionArtifactStatus,
    ExtractionDiagnosticCode, ExtractionExecutionError, ExtractionExecutionLimits,
    ExtractionExecutionOptions, ExtractionExecutor, ExtractionFailurePolicy, ExtractionFilter,
    ExtractionPlan, ExtractionPlanner, ExtractionRepresentationPolicy, ExtractionRequest,
    ExtractionRunOptions,
};
use unity_asset::workspace::{
    AssetWorkspace, WorkspaceLookup, WorkspaceObjectValue, WorkspaceView,
};
use unity_asset_decode::descriptor::{
    MediaDescriptor, MediaDimensions, MediaOutputEstimate, UnityTextureEncoding,
};

fn sample(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/samples")
        .join(name)
}

fn execution_options() -> ExtractionExecutionOptions {
    execution_options_with_in_flight(64 * 1024 * 1024)
}

fn execution_options_with_in_flight(max_in_flight_bytes: u64) -> ExtractionExecutionOptions {
    ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::new(
            1,
            max_in_flight_bytes,
            5,
            64 * 1024 * 1024,
            u64::MAX,
            16 * 1024 * 1024,
        )
        .unwrap(),
        ExistingOutputPolicy::Error,
        ExtractionFailurePolicy::CollectAll,
    )
    .unwrap()
}

#[test]
fn default_build_executes_a_persisted_decoded_plan_through_its_raw_fallback() {
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(sample("banner_1"), &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let filter = || ExtractionFilter::new([28], None, None, None).unwrap();
    let raw_plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly).with_filter(filter()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(raw_plan.artifacts().len(), 1);

    let artifact = &raw_plan.artifacts()[0];
    let raw_path = artifact.preferred_path().as_str().to_owned();
    let preferred_path = format!(
        "{}.png",
        raw_path
            .strip_suffix(".bin")
            .expect("raw plan uses the canonical binary suffix")
    );
    let fallback_path = format!(
        "{}.raw.bin",
        raw_path
            .strip_suffix(".bin")
            .expect("raw plan uses the canonical binary suffix")
    );
    let handle = match snapshot
        .resolve_object(artifact.address(), &mut AssetLoadBudget::default())
        .unwrap()
    {
        WorkspaceLookup::Resolved(handle) => handle,
        lookup => panic!("planned object must remain resolved: {lookup:?}"),
    };
    let object = snapshot
        .read_object(&handle, &mut AssetLoadBudget::default())
        .unwrap();
    let WorkspaceObjectValue::Binary(binary) = object.value() else {
        panic!("raw media fallback requires a binary object");
    };
    let expected = binary.raw_data().to_vec();

    let request =
        ExtractionRequest::all(ExtractionRepresentationPolicy::PreferDecoded).with_filter(filter());
    let descriptor = MediaDescriptor::texture_png(
        UnityTextureEncoding::Rgba32,
        MediaDimensions::new(1, 1).unwrap(),
        4,
        MediaOutputEstimate::bounded(4).unwrap(),
    )
    .unwrap();
    let mut persisted = serde_json::to_value(raw_plan).unwrap();
    persisted["request"] = serde_json::to_value(&request).unwrap();
    persisted["request_digest"] = serde_json::to_value(request.digest().unwrap()).unwrap();
    persisted["artifacts"][0]["preferred_kind"] = serde_json::json!("texture_png");
    persisted["artifacts"][0]["preferred_path"] = serde_json::json!(preferred_path);
    persisted["artifacts"][0]["preferred_content"] = serde_json::json!({
        "kind": "texture_png",
        "stream": null,
        "descriptor": descriptor,
    });
    persisted["artifacts"][0]["fallback"] = serde_json::json!({
        "kind": "binary_raw",
        "path": fallback_path.clone(),
        "content": { "kind": "raw_binary" },
    });
    let raw_working_set = u64::try_from(expected.len()).unwrap().max(1);
    persisted["artifacts"][0]["working_set_bytes"] = serde_json::Value::from(raw_working_set + 1);
    let mut downgraded = persisted.clone();
    downgraded["artifacts"][0]["preferred_kind"] = serde_json::json!("binary_raw");
    downgraded["artifacts"][0]["preferred_path"] = serde_json::json!(raw_path.clone());
    downgraded["artifacts"][0]["preferred_content"] = serde_json::json!({
        "kind": "raw_binary",
    });
    downgraded["artifacts"][0]["fallback"] = serde_json::Value::Null;
    let downgraded_address = downgraded["artifacts"][0]["address"].clone();
    downgraded["artifacts"][0]["diagnostics"] = serde_json::json!([{
        "code": "feature_unavailable",
        "address": downgraded_address,
    }]);
    let downgraded_json = serde_json::to_vec(&downgraded).unwrap();
    let downgraded_plan =
        ExtractionPlan::read_json(downgraded_json.as_slice(), &mut AssetLoadBudget::default())
            .unwrap();
    let downgraded_output = tempfile::tempdir().unwrap();
    let error = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &downgraded_plan,
            downgraded_output.path(),
            ExtractionRunOptions::new(execution_options()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ExtractionExecutionError::PlanVerification(source)
            if matches!(
                source.as_ref(),
                unity_asset::extraction::ExtractionPlanError::PlanDerivationMismatch {
                    kind: unity_asset::extraction::ExtractionPlanMismatchKind::Representations,
                }
            )
    ));

    let persisted_json = serde_json::to_vec(&persisted).unwrap();
    let plan =
        ExtractionPlan::read_json(persisted_json.as_slice(), &mut AssetLoadBudget::default())
            .unwrap();

    let output = tempfile::tempdir().unwrap();
    let report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            output.path(),
            ExtractionRunOptions::new(execution_options_with_in_flight(raw_working_set)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(report.counts().written(), 1);
    let artifact = &report.manifest().artifacts()[0];
    assert_eq!(artifact.kind(), ExtractionArtifactKind::BinaryRaw);
    assert_eq!(artifact.status(), ExtractionArtifactStatus::Written);
    assert!(
        artifact
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code() == ExtractionDiagnosticCode::FeatureUnavailable)
    );
    assert!(artifact.diagnostics().iter().all(|diagnostic| {
        diagnostic.code() != ExtractionDiagnosticCode::DecodeFailedRawFallback
    }));
    assert_eq!(
        fs::read(output.path().join(artifact.path().as_str())).unwrap(),
        expected
    );
    assert!(!output.path().join(&preferred_path).exists());

    let required = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
        .with_filter(filter());
    persisted["request"] = serde_json::to_value(&required).unwrap();
    persisted["request_digest"] = serde_json::to_value(required.digest().unwrap()).unwrap();
    persisted["artifacts"][0]["fallback"] = serde_json::Value::Null;
    let required_json = serde_json::to_vec(&persisted).unwrap();
    let required_plan =
        ExtractionPlan::read_json(required_json.as_slice(), &mut AssetLoadBudget::default())
            .unwrap();
    let required_output = tempfile::tempdir().unwrap();
    let error = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &required_plan,
            required_output.path(),
            ExtractionRunOptions::new(execution_options()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ExtractionExecutionError::PlanVerification(source)
            if matches!(
                source.as_ref(),
                unity_asset::extraction::ExtractionPlanError::ExecutionCapabilityUnavailable {
                    ordinal: 0,
                    capability: "media decode",
                }
            )
    ));
    assert!(!required_output.path().join(preferred_path).exists());
}
