use std::fs;
use std::path::PathBuf;

use unity_asset::workspace::{
    AssetWorkspace, GenericMutation, MutationPlan, PlanPayload, PrepareOptions, PublicationTarget,
    SourceExpectation, SourceOpenRequest, UnsafeRawAcknowledgement,
};
use unity_asset::{
    AssetLoadBudget, DigestV1, ObjectAddress, SourceAlias, SourceFingerprint, SourceKind,
    SourceLocator,
};
use unity_asset_binary::asset::{SerializedFile, SerializedFileParser};
use unity_asset_binary::bundle::BundleParser;

const SOURCE_ALIAS: &str = "raw-order.assets";

fn serialized_fixture() -> (Vec<u8>, SerializedFile) {
    let sample =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab");
    let bundle = BundleParser::from_bytes(fs::read(sample).unwrap()).unwrap();
    let node = bundle
        .nodes
        .iter()
        .find(|node| {
            node.is_file() && !node.name.ends_with(".resS") && !node.name.ends_with(".resource")
        })
        .unwrap();
    let bytes = bundle.extract_node_data(node).unwrap();
    let file = SerializedFileParser::from_bytes(bytes.clone()).unwrap();
    (bytes, file)
}

#[test]
fn earlier_stale_raw_guard_precedes_a_lower_path_id_failure() {
    let (bytes, file) = serialized_fixture();
    let mut path_ids = file
        .objects()
        .iter()
        .map(|object| object.path_id())
        .collect::<Vec<_>>();
    path_ids.sort_unstable();
    let low_path_id = path_ids[0];
    let high_path_id = *path_ids.last().unwrap();
    assert!(low_path_id < high_path_id);

    let stale_digest = DigestV1::hash_bytes(b"deliberately stale raw object");
    for path_id in [high_path_id, low_path_id] {
        let raw = file
            .find_object_handle(path_id)
            .unwrap()
            .raw_data()
            .unwrap();
        assert_ne!(DigestV1::hash_bytes(raw), stale_digest);
    }

    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join(SOURCE_ALIAS);
    fs::write(&source_path, &bytes).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_source(
            SourceOpenRequest::new(&source_path, SourceAlias::new(SOURCE_ALIAS).unwrap())
                .with_kind_hint(SourceKind::SerializedFile),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let locator = SourceLocator::path(SOURCE_ALIAS).unwrap();
    let payload = PlanPayload::new(vec![1, 2, 3, 4]);
    let payload_digest = payload.digest();
    let operation = |path_id| GenericMutation::UnsafeRawReplace {
        target: ObjectAddress::binary_at(locator.clone(), path_id).unwrap(),
        expected_raw_digest: stale_digest,
        payload: payload_digest,
        acknowledgement: UnsafeRawAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
    };
    let operations = vec![operation(high_path_id), operation(low_path_id)];
    let plan = MutationPlan::new(
        workspace.workspace_id(),
        workspace.revision(),
        vec![SourceExpectation::new(
            locator,
            SourceFingerprint::from_bytes(SourceKind::SerializedFile, &bytes),
        )],
        vec![payload],
        operations,
    )
    .unwrap();

    let error = workspace
        .prepare(
            plan,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    let diagnostic = &error.report().diagnostics()[0];
    assert_eq!(diagnostic.ordinal(), Some(0));
    assert_eq!(
        diagnostic.diagnostic().address(),
        Some(
            &ObjectAddress::binary_at(SourceLocator::path(SOURCE_ALIAS).unwrap(), high_path_id)
                .unwrap()
        )
    );
}

#[test]
fn repeated_identical_raw_replacement_remains_a_prepared_noop() {
    let (bytes, file) = serialized_fixture();
    let path_id = file.objects().first().unwrap().path_id();
    let raw = file
        .find_object_handle(path_id)
        .unwrap()
        .raw_data()
        .unwrap()
        .to_vec();
    let raw_digest = DigestV1::hash_bytes(&raw);

    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join(SOURCE_ALIAS);
    fs::write(&source_path, &bytes).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_source(
            SourceOpenRequest::new(&source_path, SourceAlias::new(SOURCE_ALIAS).unwrap())
                .with_kind_hint(SourceKind::SerializedFile),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let locator = SourceLocator::path(SOURCE_ALIAS).unwrap();
    let first_payload = PlanPayload::new(raw.clone());
    let first_plan = MutationPlan::new(
        workspace.workspace_id(),
        workspace.revision(),
        vec![SourceExpectation::new(
            locator.clone(),
            SourceFingerprint::from_bytes(SourceKind::SerializedFile, &bytes),
        )],
        vec![first_payload.clone()],
        vec![GenericMutation::UnsafeRawReplace {
            target: ObjectAddress::binary_at(locator.clone(), path_id).unwrap(),
            expected_raw_digest: raw_digest,
            payload: first_payload.digest(),
            acknowledgement: UnsafeRawAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
        }],
    )
    .unwrap();

    let first = workspace
        .prepare(
            first_plan,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    workspace
        .commit(
            first,
            PublicationTarget::in_place(directory.path()).unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let canonical = fs::read(&source_path).unwrap();
    let second_payload = PlanPayload::new(raw);
    let second_plan = MutationPlan::new(
        workspace.workspace_id(),
        workspace.revision(),
        vec![SourceExpectation::new(
            locator.clone(),
            SourceFingerprint::from_bytes(SourceKind::SerializedFile, &canonical),
        )],
        vec![second_payload.clone()],
        vec![GenericMutation::UnsafeRawReplace {
            target: ObjectAddress::binary_at(locator, path_id).unwrap(),
            expected_raw_digest: raw_digest,
            payload: second_payload.digest(),
            acknowledgement: UnsafeRawAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
        }],
    )
    .unwrap();
    let prepared = workspace
        .prepare(
            second_plan,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(
        prepared.report().base_revision(),
        prepared.report().prepared_revision()
    );
}
