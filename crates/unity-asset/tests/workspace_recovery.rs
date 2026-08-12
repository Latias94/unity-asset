use std::fs;

use unity_asset::workspace::{
    AssetWorkspace, FieldGuard, GenericMutation, MutationPlan, MutationValue, PrepareOptions,
    PublicationTarget, RecoveryDiscoveryBlockedReason, RecoveryDiscoveryError, RecoveryOutcome,
    SourceExpectation, SourceOpenRequest, WorkspaceOptions,
};
use unity_asset::{
    AssetLoadBudget, AssetLoadLimits, DigestV1, FieldPath, ObjectAddress, SourceAlias,
    SourceFingerprint, SourceKind, SourceLocator, TransactionId, UnityClass, UnityValue,
};
use unity_asset_core::{semantic_value_digest, yaml_field_schema_digest};

const SOURCE_ALIAS: &str = "recovery.prefab";
const YAML: &[u8] =
    b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Before\n";

fn mutation_plan(workspace: &AssetWorkspace) -> MutationPlan {
    mutation_plan_with_values(workspace, YAML, "Before", "After")
}

fn mutation_plan_with_values(
    workspace: &AssetWorkspace,
    source_bytes: &[u8],
    before_name: &str,
    replacement_name: &str,
) -> MutationPlan {
    let path = FieldPath::root().push_field("m_Name").unwrap();
    let class = UnityClass::new(1, "GameObject".to_owned(), "1".to_owned());
    let before = UnityValue::String(before_name.to_owned());
    let mut budget = AssetLoadBudget::default();
    let guard = FieldGuard::new(
        yaml_field_schema_digest(&class, &path, &before, &mut budget).unwrap(),
        semantic_value_digest(&before, &mut budget).unwrap(),
    );
    MutationPlan::new(
        workspace.workspace_id(),
        workspace.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(SOURCE_ALIAS).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, source_bytes),
        )],
        Vec::new(),
        vec![GenericMutation::FieldReplace {
            target: ObjectAddress::yaml(
                SourceLocator::path(SOURCE_ALIAS).unwrap(),
                "1".parse().unwrap(),
            )
            .unwrap(),
            path,
            guard,
            replacement: MutationValue::string(replacement_name).unwrap(),
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
        RecoveryOutcome::HistoricalCommitReceipt(Box::new(report.clone()))
    );
    assert!(!recovered.requires_workspace_finalization());
    assert_eq!(recovered.historical_commit_receipt(), Some(&report));

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
fn public_recovery_discovery_is_empty_and_zero_write_on_a_clean_root() {
    let directory = tempfile::tempdir().unwrap();
    let target = PublicationTarget::in_place(directory.path()).unwrap();
    let mut budget = AssetLoadBudget::default();
    let before = budget.usage();

    let discovery = target.discover_recoveries(&mut budget).unwrap();

    assert_eq!(discovery.version(), 1);
    assert!(discovery.is_empty());
    assert_eq!(discovery.len(), 0);
    assert!(discovery.recoveries().is_empty());
    assert_eq!(budget.usage().entries, before.entries);
    assert!(budget.usage().bytes > before.bytes);
    assert_eq!(budget.usage().members, before.members);
    assert!(!directory.path().join(".unity-asset-recovery").exists());
}

#[test]
fn public_recovery_discovery_blocks_a_replaced_publication_root() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("published");
    fs::create_dir(&root).unwrap();
    let target = PublicationTarget::in_place(&root).unwrap();
    fs::rename(&root, directory.path().join("original-published")).unwrap();
    fs::create_dir(&root).unwrap();

    let error = target
        .discover_recoveries(&mut AssetLoadBudget::default())
        .unwrap_err();
    assert!(matches!(
        error,
        RecoveryDiscoveryError::Blocked {
            reason: RecoveryDiscoveryBlockedReason::UnsafeFilesystemState,
        }
    ));
}

#[test]
fn public_recovery_discovery_deduplicates_and_sorts_canonical_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(SOURCE_ALIAS);
    fs::write(&path, YAML).unwrap();
    let mut workspace = open_workspace(&path, None);
    let target = PublicationTarget::in_place(directory.path()).unwrap();
    let prepared = workspace
        .prepare(
            mutation_plan(&workspace),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let report = workspace
        .commit(prepared, target.clone(), &mut AssetLoadBudget::default())
        .unwrap();

    let preparation = report.recovery().root().with_extension("prepare.v2.json");
    fs::write(&preparation, b"not valid JSON").unwrap();
    let additional = TransactionId::new(DigestV1::hash_bytes(b"additional discovery evidence"));
    let additional_locator = target.recovery_locator(additional);
    fs::create_dir(additional_locator.root()).unwrap();

    let discovery = target
        .discover_recoveries(&mut AssetLoadBudget::default())
        .unwrap();
    let mut expected = vec![report.transaction(), additional];
    expected.sort_unstable();
    let actual = discovery
        .recoveries()
        .iter()
        .map(|locator| locator.transaction())
        .collect::<Vec<_>>();

    assert_eq!(actual, expected);
}

#[test]
fn public_recovery_discovery_blocks_unknown_evidence_and_budget_exhaustion() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(SOURCE_ALIAS);
    fs::write(&path, YAML).unwrap();
    let mut workspace = open_workspace(&path, None);
    let target = PublicationTarget::in_place(directory.path()).unwrap();
    let prepared = workspace
        .prepare(
            mutation_plan(&workspace),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let report = workspace
        .commit(prepared, target.clone(), &mut AssetLoadBudget::default())
        .unwrap();
    let before = fs::read(&path).unwrap();

    let mut exhausted = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let budget_error = target.discover_recoveries(&mut exhausted).unwrap_err();
    assert!(matches!(
        budget_error,
        RecoveryDiscoveryError::Budget { .. }
    ));
    assert_eq!(fs::read(&path).unwrap(), before);

    let version_root = report
        .recovery()
        .root()
        .parent()
        .expect("recovery version root");
    fs::write(version_root.join("unexpected-evidence"), b"x").unwrap();
    let error = target
        .discover_recoveries(&mut AssetLoadBudget::default())
        .unwrap_err();
    assert!(matches!(error, RecoveryDiscoveryError::Blocked { .. }));
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn public_recovery_discovery_obeys_exact_and_one_short_budgets() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(SOURCE_ALIAS);
    fs::write(&path, YAML).unwrap();
    let mut workspace = open_workspace(&path, None);
    let target = PublicationTarget::in_place(directory.path()).unwrap();
    let prepared = workspace
        .prepare(
            mutation_plan(&workspace),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    workspace
        .commit(prepared, target.clone(), &mut AssetLoadBudget::default())
        .unwrap();
    let original = fs::read(&path).unwrap();

    let mut measured = AssetLoadBudget::default();
    let measured_discovery = target.discover_recoveries(&mut measured).unwrap();
    let usage = measured.usage();
    assert!(usage.bytes > 0);

    let exact_limits = AssetLoadLimits {
        max_entries: usage.entries,
        max_bytes: usage.bytes,
        max_members: usage.members,
        ..AssetLoadLimits::default()
    };
    let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
    assert_eq!(
        target.discover_recoveries(&mut exact).unwrap(),
        measured_discovery
    );

    let one_short_limits = AssetLoadLimits {
        max_entries: usage.entries,
        max_bytes: usage.bytes - 1,
        max_members: usage.members,
        ..AssetLoadLimits::default()
    };
    let mut one_short = AssetLoadBudget::new(one_short_limits).unwrap();
    let error = target.discover_recoveries(&mut one_short).unwrap_err();
    assert!(matches!(error, RecoveryDiscoveryError::Budget { .. }));
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn finalized_receipt_does_not_overwrite_a_successor_commit() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(SOURCE_ALIAS);
    fs::write(&path, YAML).unwrap();
    let mut workspace = open_workspace(&path, None);
    let target = PublicationTarget::in_place(directory.path()).unwrap();

    let first_prepared = workspace
        .prepare(
            mutation_plan(&workspace),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let first = workspace
        .commit(
            first_prepared,
            target.clone(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let second_prepared = workspace
        .prepare(
            mutation_plan_with_values(&workspace, &fs::read(&path).unwrap(), "After", "Later"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let second = workspace
        .commit(second_prepared, target, &mut AssetLoadBudget::default())
        .unwrap();
    let successor_bytes = fs::read(&path).unwrap();

    let detached =
        AssetWorkspace::recover_at(first.recovery(), &mut AssetLoadBudget::default()).unwrap();
    assert_eq!(
        detached,
        RecoveryOutcome::HistoricalCommitReceipt(Box::new(first.clone()))
    );
    assert_eq!(fs::read(&path).unwrap(), successor_bytes);

    let attached = workspace
        .finalize_recovery_at(first.recovery(), &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(
        attached,
        RecoveryOutcome::HistoricalCommitReceipt(Box::new(first))
    );
    assert_eq!(workspace.revision(), second.committed_revision());
    assert_eq!(fs::read(&path).unwrap(), successor_bytes);
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
