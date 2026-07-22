use std::fs;

use unity_asset::workspace::{
    AssetWorkspace, FieldGuard, GenericMutation, MutationPlan, MutationValue, PrepareOptions,
    PublicationTarget, RecoveryOutcome, SourceExpectation, SourceOpenRequest, WorkspaceOptions,
};
use unity_asset::{
    AssetLoadBudget, DigestV1, FieldPath, ObjectAddress, SourceAlias, SourceFingerprint,
    SourceKind, SourceLocator, TransactionId, UnityClass, UnityValue,
};
use unity_asset_core::{semantic_value_digest, yaml_field_schema_digest};

const SOURCE_ALIAS: &str = "recovery.prefab";
const YAML: &[u8] =
    b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Before\n";

fn mutation_plan(workspace: &AssetWorkspace) -> MutationPlan {
    let path = FieldPath::root().push_field("m_Name").unwrap();
    let class = UnityClass::new(1, "GameObject".to_owned(), "1".to_owned());
    let before = UnityValue::String("Before".to_owned());
    let mut budget = AssetLoadBudget::default();
    let guard = FieldGuard::new(
        yaml_field_schema_digest(&class, &path, &before, &mut budget).unwrap(),
        semantic_value_digest(&before, &mut budget).unwrap(),
    );
    MutationPlan::new(
        workspace.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(SOURCE_ALIAS).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, YAML),
        )],
        Vec::new(),
        vec![GenericMutation::FieldReplace {
            target: ObjectAddress::yaml(SourceLocator::path(SOURCE_ALIAS).unwrap(), "1").unwrap(),
            path,
            guard,
            replacement: MutationValue::string("After").unwrap(),
        }],
    )
    .unwrap()
}

fn open_workspace(
    path: &std::path::Path,
    workspace_id: Option<unity_asset::WorkspaceId>,
) -> AssetWorkspace {
    let mut workspace = match workspace_id {
        Some(workspace_id) => {
            AssetWorkspace::with_workspace_id(workspace_id, WorkspaceOptions::default()).unwrap()
        }
        None => AssetWorkspace::new().unwrap(),
    };
    workspace
        .load_source(
            SourceOpenRequest::new(path, SourceAlias::new(SOURCE_ALIAS).unwrap())
                .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    workspace
}

#[test]
fn public_recovery_contract_separates_filesystem_convergence_from_finalization() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(SOURCE_ALIAS);
    fs::write(&path, YAML).unwrap();
    let mut workspace = open_workspace(&path, None);
    let prepared = workspace
        .prepare(
            mutation_plan(&workspace),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let report = workspace
        .commit(
            prepared,
            PublicationTarget::in_place(directory.path()).unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    drop(workspace);

    let recovered =
        AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default()).unwrap();
    assert_eq!(
        recovered,
        RecoveryOutcome::FilesystemRecovered(Box::new(report.clone()))
    );
    assert!(recovered.requires_workspace_finalization());

    let mut reopened = open_workspace(&path, recovered.workspace_id());
    let finalized = reopened
        .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(
        finalized,
        RecoveryOutcome::Finalized(Box::new(report.clone()))
    );
    assert!(!finalized.requires_workspace_finalization());
    assert_eq!(reopened.revision(), report.committed_revision());
}

#[test]
fn public_recovery_contract_reports_absent_transactions_on_a_clean_root() {
    let directory = tempfile::tempdir().unwrap();
    let transaction = TransactionId::new(DigestV1::hash_bytes(b"absent transaction"));
    let locator = PublicationTarget::in_place(directory.path())
        .unwrap()
        .recovery_locator(transaction);

    let outcome = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default()).unwrap();

    assert_eq!(outcome, RecoveryOutcome::NoTransaction(locator));
    assert_eq!(outcome.workspace_id(), None);
    assert!(!outcome.requires_workspace_finalization());
}
