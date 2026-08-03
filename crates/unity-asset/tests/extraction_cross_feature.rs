#![cfg(not(feature = "decode"))]

use std::fs;
use std::path::PathBuf;

use unity_asset::AssetLoadBudget;
use unity_asset::extraction::{
    ExistingOutputPolicy, ExtractionArtifactKind, ExtractionArtifactStatus,
    ExtractionExecutionError, ExtractionExecutionLimits, ExtractionExecutionOptions,
    ExtractionExecutor, ExtractionFailurePolicy, ExtractionFilter, ExtractionPlan,
    ExtractionPlanner, ExtractionRepresentationPolicy, ExtractionRequest, ExtractionRunOptions,
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
    ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::new(1, 64 * 1024 * 1024, 5, 64 * 1024 * 1024, 16 * 1024 * 1024)
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
        "path": raw_path,
        "content": { "kind": "raw_binary" },
    });
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
            ExtractionRunOptions::new(execution_options()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(report.counts().written(), 1);
    let artifact = &report.manifest().artifacts()[0];
    assert_eq!(artifact.kind(), ExtractionArtifactKind::BinaryRaw);
    assert_eq!(artifact.status(), ExtractionArtifactStatus::Written);
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
        ExtractionExecutionError::MediaPreparationFailed { ordinal: 0 }
    ));
    assert!(!required_output.path().join(preferred_path).exists());
}
