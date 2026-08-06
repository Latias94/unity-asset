use std::fs;
use std::path::Path;

use super::source_state::{SOURCE_STATE_CONTRACT_VERSION, SOURCE_STATE_LOGICAL_IDENTITY_VERSION};
use super::{
    GenerationActivationEvidence, GenerationBuild, GenerationFailpoint,
    GenerationPublishWarningKind, GenerationStore, GenerationStoreError, GenerationStoreOptions,
    TransactionReceiptWindow, activation_file_name, activation_staging_file_name,
    quarantine_directory_name, staging_directory_name,
};
use crate::generation::{
    ArtifactTreeEvidence, GenerationArtifactEvidence, GenerationProjectionDigests,
    GenerationStorageContract, SEARCH_GENERATION_STORAGE_CONTRACT_VERSION, SearchGenerationId,
    SearchGenerationIdentityV1, SearchGenerationManifestV1, StoredGenerationRef,
};
use crate::semantics::SearchSemantics;
use serde::Serialize;
use tempfile::TempDir;
use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, AssetLoadUsage, BudgetError, BudgetedJsonError, ChangeSet,
    DigestV1, SourceId, SourceKind, TransactionId, WorkspaceId, WorkspaceRevision,
};

fn digest(label: &str) -> DigestV1 {
    DigestV1::hash_bytes(label.as_bytes())
}

fn revision(label: &str) -> WorkspaceRevision {
    WorkspaceRevision::new(digest(label))
}

fn change_set(
    label: &str,
    from_revision: WorkspaceRevision,
    to_revision: WorkspaceRevision,
    source_local: u128,
) -> ChangeSet {
    let workspace = WorkspaceId::from_u128(0x9001).unwrap();
    ChangeSet::new(
        TransactionId::new(digest(label)),
        workspace,
        from_revision,
        to_revision,
        vec![SourceId::new(workspace, SourceKind::SerializedFile, source_local).unwrap()],
        Vec::new(),
        Vec::new(),
    )
    .unwrap()
}

fn receipt_window(changes: &ChangeSet) -> TransactionReceiptWindow {
    TransactionReceiptWindow::from_change_set(changes, &mut AssetLoadBudget::default()).unwrap()
}

fn open_store(
    root: impl AsRef<Path>,
    options: GenerationStoreOptions,
) -> Result<GenerationStore, GenerationStoreError> {
    GenerationStore::open(root, options, &mut AssetLoadBudget::default())
}

fn budget_for_usage(usage: AssetLoadUsage, max_bytes: u64) -> AssetLoadBudget {
    AssetLoadBudget::new(AssetLoadLimits {
        max_entries: usage.entries.max(1),
        max_bytes,
        max_depth: usage.max_observed_depth,
        max_members: usage.members.max(1),
        ..AssetLoadLimits::default()
    })
    .unwrap()
}

fn assert_byte_budget_error(error: GenerationStoreError) {
    assert!(
        matches!(
            &error,
            GenerationStoreError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }) | GenerationStoreError::ContractJson {
                source: BudgetedJsonError::Budget(BudgetError::Exceeded {
                    resource: "bytes",
                    ..
                }),
                ..
            }
        ),
        "unexpected generation-store budget error: {error:?}"
    );
}

#[cfg(windows)]
fn create_junction(link: &std::path::Path, target: &std::path::Path) {
    use std::process::Command;

    let status = Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .status()
        .unwrap();
    assert!(status.success());
}

fn source_state_payload_for_workspace(label: &str, workspace: WorkspaceId) -> (Vec<u8>, DigestV1) {
    #[derive(Serialize)]
    struct LogicalState<'state> {
        identity_version: u16,
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        analysis_cache_identity: crate::semantics::AnalysisCacheIdentityV1,
        assets: &'state [()],
    }

    #[derive(Serialize)]
    struct PersistedState<'state> {
        contract_version: u16,
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        analysis_cache_identity: crate::semantics::AnalysisCacheIdentityV1,
        scan_hints: &'state [()],
        assets: &'state [()],
        logical_digest: DigestV1,
    }

    let revision = revision(label);
    let analysis_cache_identity = SearchSemantics::current()
        .analysis_cache_identity(digest("options"))
        .unwrap();
    let assets = [];
    let logical = LogicalState {
        identity_version: SOURCE_STATE_LOGICAL_IDENTITY_VERSION,
        workspace,
        revision,
        analysis_cache_identity,
        assets: &assets,
    };
    let logical_digest = DigestV1::hash_bytes(&serde_json::to_vec(&logical).unwrap());
    let persisted = PersistedState {
        contract_version: SOURCE_STATE_CONTRACT_VERSION,
        workspace,
        revision,
        analysis_cache_identity,
        scan_hints: &[],
        assets: &assets,
        logical_digest,
    };
    (serde_json::to_vec(&persisted).unwrap(), logical_digest)
}

struct LegacyGenerationFixture {
    generation: SearchGenerationId,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    desired_revision: WorkspaceRevision,
}

fn install_frozen_legacy_generation(
    root: &Path,
    activation_contract_version: u16,
    desired_revision: WorkspaceRevision,
) -> LegacyGenerationFixture {
    assert!((1..=3).contains(&activation_contract_version));
    crate::legacy_storage_v1_fixture_tests::install_frozen_storage_v1_store(root);
    let activation_path = root
        .join(super::ACTIVATIONS_DIRECTORY)
        .join(activation_file_name(1));
    let mut activation: serde_json::Value =
        serde_json::from_slice(&fs::read(&activation_path).unwrap()).unwrap();
    let generation = serde_json::from_value(activation["generation"].clone()).unwrap();
    let workspace = serde_json::from_value(activation["workspace"].clone()).unwrap();
    let revision = serde_json::from_value(activation["revision"].clone()).unwrap();
    let activation_fields = activation.as_object_mut().unwrap();
    activation_fields.insert(
        "contract_version".to_owned(),
        serde_json::json!(activation_contract_version),
    );
    activation_fields.remove("generation_storage_contract");
    activation_fields.remove("parent_generation");
    activation_fields.remove("transaction_receipts");
    if activation_contract_version >= 2 {
        activation_fields.insert(
            "desired_revision".to_owned(),
            serde_json::to_value(desired_revision).unwrap(),
        );
    } else {
        activation_fields.remove("desired_revision");
    }
    if activation_contract_version == 3 {
        activation_fields.insert(
            "generation_storage_contract".to_owned(),
            serde_json::to_value(GenerationStorageContract::LegacyV1).unwrap(),
        );
        activation_fields.insert(
            "transaction_receipts".to_owned(),
            serde_json::to_value(TransactionReceiptWindow::empty()).unwrap(),
        );
    }
    fs::write(activation_path, serde_json::to_vec(&activation).unwrap()).unwrap();

    LegacyGenerationFixture {
        generation,
        workspace,
        revision,
        desired_revision: if activation_contract_version == 1 {
            revision
        } else {
            desired_revision
        },
    }
}

fn write_artifacts_for_workspace(build: &GenerationBuild, label: &str, workspace: WorkspaceId) {
    fs::write(
        build.search_directory().join("segments"),
        format!("search:{label}"),
    )
    .unwrap();
    fs::write(
        build.reference_directory().join("segments"),
        format!("references:{label}"),
    )
    .unwrap();
    fs::write(
        build.source_state_directory().join("source-state-v2.json"),
        source_state_payload_for_workspace(label, workspace).0,
    )
    .unwrap();
}

fn write_artifacts(build: &GenerationBuild, label: &str) {
    write_artifacts_for_workspace(build, label, WorkspaceId::from_u128(0x9001).unwrap());
}

fn manifest_for_workspace(
    store: &GenerationStore,
    build: &GenerationBuild,
    label: &str,
    expected_parent: Option<SearchGenerationId>,
    workspace: WorkspaceId,
) -> SearchGenerationManifestV1 {
    assert_eq!(
        store.active().map(super::GenerationSnapshot::generation),
        expected_parent
    );
    let evidence = store.measure_artifacts(build).unwrap();
    let identity = SearchGenerationIdentityV1::new(
        workspace,
        revision(label),
        GenerationProjectionDigests::new(
            digest(&format!("search-projection:{label}")),
            digest(&format!("reference-projection:{label}")),
        ),
        Default::default(),
        digest("options"),
        source_state_payload_for_workspace(label, workspace).1,
    )
    .unwrap();
    SearchGenerationManifestV1::new(identity, evidence)
}

fn manifest_for(
    store: &GenerationStore,
    build: &GenerationBuild,
    label: &str,
    expected_parent: Option<SearchGenerationId>,
) -> SearchGenerationManifestV1 {
    manifest_for_workspace(
        store,
        build,
        label,
        expected_parent,
        WorkspaceId::from_u128(0x9001).unwrap(),
    )
}

fn publish_generation(
    store: &mut GenerationStore,
    label: &str,
    parent: Option<SearchGenerationId>,
) -> SearchGenerationId {
    publish_generation_for_workspace(
        store,
        label,
        parent,
        WorkspaceId::from_u128(0x9001).unwrap(),
    )
}

fn publish_generation_for_workspace(
    store: &mut GenerationStore,
    label: &str,
    parent: Option<SearchGenerationId>,
    workspace: WorkspaceId,
) -> SearchGenerationId {
    let mut build = store.begin().unwrap();
    write_artifacts_for_workspace(&build, label, workspace);
    let manifest = manifest_for_workspace(store, &build, label, parent, workspace);
    let prepared = store.prepare_publish(&mut build, manifest).unwrap();
    assert!(prepared.snapshot().directory().is_dir());
    prepared.activate().unwrap().active.generation()
}

#[test]
fn writer_lease_rejects_a_second_store_until_the_first_drops() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let build = store.begin().unwrap();

    assert!(matches!(
        open_store(temporary.path(), GenerationStoreOptions::default()),
        Err(GenerationStoreError::WriterLeaseUnavailable { .. })
    ));

    drop(store);
    assert!(matches!(
        open_store(temporary.path(), GenerationStoreOptions::default()),
        Err(GenerationStoreError::WriterLeaseUnavailable { .. })
    ));
    drop(build);
    open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
}

#[test]
fn store_allows_only_one_armed_build_and_releases_the_claim_on_abort() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let mut first = store.begin().unwrap();
    let first_path = first.directory().to_path_buf();

    assert!(matches!(
        store.begin(),
        Err(GenerationStoreError::BuildAlreadyActive)
    ));

    first
        .abort_with_budget(&mut AssetLoadBudget::default())
        .unwrap();
    assert!(!first_path.exists());

    let second = store.begin().unwrap();
    let second_path = second.directory().to_path_buf();
    drop(second);
    assert!(!second_path.exists());
}

#[test]
fn failed_explicit_abort_relinquishes_the_claim_for_later_recovery() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let mut first = store.begin().unwrap();
    let first_path = first.directory().to_path_buf();
    fs::write(first.search_directory().join("partial"), b"partial").unwrap();
    let mut insufficient = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    assert!(matches!(
        first.abort_with_budget(&mut insufficient),
        Err(GenerationStoreError::Budget(BudgetError::Exceeded {
            resource: "entries",
            ..
        }))
    ));
    assert!(first_path.exists());

    let replacement = store.begin().unwrap();
    drop(replacement);
    let report = store
        .reconcile_abandoned_staging(&mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(report.removed_entries(), 1);
    assert!(!first_path.exists());
}

#[test]
fn desired_revision_head_survives_reopen_and_new_generation_clears_stale_state() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    let mut store = open_store(temporary.path(), options).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);
    let baseline_ordinal = store.active().unwrap().activation_ordinal();
    let desired = revision("desired");

    store
        .record_desired_revision(desired, &mut AssetLoadBudget::default())
        .unwrap();
    let stale = store.active().unwrap();
    assert_eq!(stale.generation(), baseline);
    assert_eq!(stale.manifest().revision(), revision("baseline"));
    assert_eq!(stale.desired_revision(), desired);
    assert!(stale.activation_ordinal() > baseline_ordinal);

    drop(store);
    let mut reopened = open_store(temporary.path(), options).unwrap();
    let recovered = reopened.active().unwrap();
    assert_eq!(recovered.generation(), baseline);
    assert_eq!(recovered.manifest().revision(), revision("baseline"));
    assert_eq!(recovered.desired_revision(), desired);

    let current = publish_generation(&mut reopened, "desired", Some(baseline));
    let fresh = reopened.active().unwrap();
    assert_eq!(fresh.generation(), current);
    assert_eq!(fresh.manifest().revision(), desired);
    assert_eq!(fresh.desired_revision(), desired);

    drop(reopened);
    let recovered = open_store(temporary.path(), options).unwrap();
    let fresh = recovered.active().unwrap();
    assert_eq!(fresh.generation(), current);
    assert_eq!(fresh.manifest().revision(), desired);
    assert_eq!(fresh.desired_revision(), desired);
}

#[test]
fn corrupt_latest_desired_revision_head_fails_closed() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    publish_generation(&mut store, "actual", None);
    store
        .record_desired_revision(revision("desired"), &mut AssetLoadBudget::default())
        .unwrap();
    let ordinal = store.active().unwrap().activation_ordinal();
    let head = temporary
        .path()
        .join("activations")
        .join(activation_file_name(ordinal));
    drop(store);
    fs::write(head, b"{").unwrap();

    let error = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap_err();
    assert!(matches!(
        error,
        GenerationStoreError::ContractJson {
            artifact: "activation record",
            ..
        }
    ));
}

#[test]
fn corrupt_historical_head_does_not_hide_a_valid_latest_head() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 1,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    let first = publish_generation(&mut store, "first", None);
    let first_ordinal = store.active().unwrap().activation_ordinal();
    let second = publish_generation(&mut store, "second", Some(first));
    let historical = temporary
        .path()
        .join("activations")
        .join(activation_file_name(first_ordinal));
    assert!(historical.is_file());
    drop(store);
    fs::write(historical, b"{").unwrap();

    let reopened = open_store(temporary.path(), options).unwrap();
    assert_eq!(reopened.active().unwrap().generation(), second);
}

#[test]
fn legacy_activation_v1_and_v2_open_exact_storage_v1_generations() {
    for activation_contract_version in [1, 2] {
        let temporary = TempDir::new().unwrap();
        let options = GenerationStoreOptions::default();
        drop(open_store(temporary.path(), options).unwrap());
        let fixture = install_frozen_legacy_generation(
            temporary.path(),
            activation_contract_version,
            revision("legacy desired"),
        );
        let activation: serde_json::Value = serde_json::from_slice(
            &fs::read(
                temporary
                    .path()
                    .join(super::ACTIVATIONS_DIRECTORY)
                    .join(activation_file_name(1)),
            )
            .unwrap(),
        )
        .unwrap();
        let activation = activation.as_object().unwrap();
        assert!(!activation.contains_key("generation_storage_contract"));
        assert!(!activation.contains_key("parent_generation"));
        assert!(!activation.contains_key("transaction_receipts"));
        assert_eq!(
            activation.contains_key("desired_revision"),
            activation_contract_version == 2
        );

        let reopened = open_store(temporary.path(), options).unwrap();
        let active = reopened.active().unwrap();
        assert_eq!(active.generation(), fixture.generation);
        assert_eq!(
            active.storage_contract(),
            GenerationStorageContract::LegacyV1
        );
        assert_eq!(active.manifest().workspace(), fixture.workspace);
        assert_eq!(active.manifest().revision(), fixture.revision);
        assert_eq!(active.desired_revision(), fixture.desired_revision);
        assert!(
            active
                .directory()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("generation-v1-")
        );
        assert!(matches!(
            active
                .load_source_state(&mut AssetLoadBudget::default())
                .unwrap(),
            super::GenerationSourceState::LegacyV1(_)
        ));
    }
}

#[test]
fn legacy_activation_contracts_reject_fields_outside_their_exact_wire() {
    let cases = [
        (1, "desired_revision", serde_json::Value::Null),
        (1, "generation_storage_contract", serde_json::Value::Null),
        (1, "parent_generation", serde_json::Value::Null),
        (1, "transaction_receipts", serde_json::json!([])),
        (1, "transaction_receipts", serde_json::Value::Null),
        (2, "generation_storage_contract", serde_json::Value::Null),
        (2, "parent_generation", serde_json::Value::Null),
        (2, "transaction_receipts", serde_json::json!([])),
        (2, "transaction_receipts", serde_json::Value::Null),
    ];

    for (contract_version, field, value) in cases {
        let temporary = TempDir::new().unwrap();
        drop(open_store(temporary.path(), GenerationStoreOptions::default()).unwrap());
        install_frozen_legacy_generation(
            temporary.path(),
            contract_version,
            revision("legacy desired"),
        );
        let activation_path = temporary
            .path()
            .join(super::ACTIVATIONS_DIRECTORY)
            .join(activation_file_name(1));
        let mut activation: serde_json::Value =
            serde_json::from_slice(&fs::read(&activation_path).unwrap()).unwrap();
        activation
            .as_object_mut()
            .unwrap()
            .insert(field.to_owned(), value);
        fs::write(&activation_path, serde_json::to_vec(&activation).unwrap()).unwrap();

        let error = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::InvalidGenerationHead { .. }
        ));
    }
}

#[test]
fn activation_v2_requires_a_non_null_desired_revision() {
    for replacement in [None, Some(serde_json::Value::Null)] {
        let temporary = TempDir::new().unwrap();
        drop(open_store(temporary.path(), GenerationStoreOptions::default()).unwrap());
        install_frozen_legacy_generation(temporary.path(), 2, revision("legacy desired"));
        let activation_path = temporary
            .path()
            .join(super::ACTIVATIONS_DIRECTORY)
            .join(activation_file_name(1));
        let mut activation: serde_json::Value =
            serde_json::from_slice(&fs::read(&activation_path).unwrap()).unwrap();
        let fields = activation.as_object_mut().unwrap();
        match replacement {
            Some(value) => {
                fields.insert("desired_revision".to_owned(), value);
            }
            None => {
                fields.remove("desired_revision");
            }
        }
        fs::write(&activation_path, serde_json::to_vec(&activation).unwrap()).unwrap();

        let error = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::InvalidGenerationHead { .. }
        ));
    }
}

#[test]
fn activation_v3_requires_exact_non_null_current_fields() {
    let cases = [
        ("generation_storage_contract", None),
        ("generation_storage_contract", Some(serde_json::Value::Null)),
        ("desired_revision", None),
        ("desired_revision", Some(serde_json::Value::Null)),
        ("transaction_receipts", None),
        ("transaction_receipts", Some(serde_json::Value::Null)),
        ("parent_generation", Some(serde_json::Value::Null)),
    ];

    for (field, replacement) in cases {
        let temporary = TempDir::new().unwrap();
        let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
        publish_generation(&mut store, &format!("current-{field}"), None);
        let ordinal = store.active().unwrap().activation_ordinal();
        let activation_path = temporary
            .path()
            .join(super::ACTIVATIONS_DIRECTORY)
            .join(activation_file_name(ordinal));
        drop(store);

        let mut activation: serde_json::Value =
            serde_json::from_slice(&fs::read(&activation_path).unwrap()).unwrap();
        let fields = activation.as_object_mut().unwrap();
        match replacement {
            Some(value) => {
                fields.insert(field.to_owned(), value);
            }
            None => {
                fields.remove(field);
            }
        }
        fs::write(&activation_path, serde_json::to_vec(&activation).unwrap()).unwrap();

        let error = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::InvalidGenerationHead { .. }
        ));
    }
}

#[test]
fn activation_v3_can_pin_legacy_storage_and_a_newer_desired_revision() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    drop(open_store(temporary.path(), options).unwrap());
    let desired_revision = revision("legacy-v3-desired");
    let fixture = install_frozen_legacy_generation(temporary.path(), 3, desired_revision);

    let reopened = open_store(temporary.path(), options).unwrap();
    let active = reopened.active().unwrap();
    assert_eq!(active.generation(), fixture.generation);
    assert_eq!(
        active.storage_contract(),
        GenerationStorageContract::LegacyV1
    );
    assert_eq!(active.desired_revision(), desired_revision);
}

#[test]
fn desired_revision_head_budget_failure_is_precommit() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let generation = publish_generation(&mut store, "baseline", None);
    let actual_revision = store.active().unwrap().manifest().revision();
    let activation_count = fs::read_dir(temporary.path().join("activations"))
        .unwrap()
        .count();
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    let error = store
        .record_desired_revision(revision("desired"), &mut budget)
        .unwrap_err();
    assert_byte_budget_error(error);
    assert_eq!(
        fs::read_dir(temporary.path().join("activations"))
            .unwrap()
            .count(),
        activation_count
    );
    let active = store.active().unwrap();
    assert_eq!(active.generation(), generation);
    assert_eq!(active.desired_revision(), actual_revision);
}

#[test]
fn dropped_builds_and_reopened_stores_recover_owned_staging() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let dropped_path = {
        let build = store.begin().unwrap();
        let path = build.directory().to_path_buf();
        fs::write(build.search_directory().join("partial"), b"partial").unwrap();
        path
    };
    assert!(!dropped_path.exists());
    drop(store);

    let abandoned = temporary
        .path()
        .join(".staging")
        .join("build-00000000000000000099");
    fs::create_dir(&abandoned).unwrap();
    fs::write(abandoned.join("partial"), b"partial").unwrap();

    let reopened = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    assert!(!abandoned.exists());
    drop(reopened);
}

#[test]
fn reconciliation_removes_only_abandoned_staging_and_preserves_the_active_generation() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let active = publish_generation(&mut store, "active", None);
    let active_directory = store.generation_directory(active);
    let active_activation = temporary
        .path()
        .join("activations")
        .join(activation_file_name(
            store.active().unwrap().activation_ordinal(),
        ));
    let staging = temporary.path().join(".staging");
    let abandoned_build = staging.join(staging_directory_name(91));
    let abandoned_quarantine = staging.join(quarantine_directory_name(92, active));
    let abandoned_activation = staging.join(activation_staging_file_name(93));
    fs::create_dir(&abandoned_build).unwrap();
    fs::write(abandoned_build.join("partial"), b"partial").unwrap();
    fs::create_dir(&abandoned_quarantine).unwrap();
    fs::write(abandoned_quarantine.join("old"), b"old").unwrap();
    fs::write(&abandoned_activation, b"partial activation").unwrap();

    let live_build = store.begin().unwrap();
    assert!(matches!(
        store.reconcile_abandoned_staging(&mut AssetLoadBudget::default()),
        Err(GenerationStoreError::BuildAlreadyActive)
    ));
    drop(live_build);

    let report = store
        .reconcile_abandoned_staging(&mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(report.removed_entries(), 3);
    assert!(!abandoned_build.exists());
    assert!(!abandoned_quarantine.exists());
    assert!(!abandoned_activation.exists());
    assert!(active_directory.is_dir());
    assert!(active_activation.is_file());
    assert_eq!(store.active().unwrap().generation(), active);
}

#[cfg(unix)]
#[test]
fn staging_recovery_rejects_nested_links_without_touching_the_target() {
    use std::os::unix::fs::symlink;

    let temporary = TempDir::new().unwrap();
    let store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    drop(store);

    let target = temporary.path().join("external-target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("sentinel"), b"keep").unwrap();
    let abandoned = temporary
        .path()
        .join(".staging")
        .join("build-00000000000000000099");
    fs::create_dir(&abandoned).unwrap();
    symlink(&target, abandoned.join("escape")).unwrap();

    assert!(matches!(
        open_store(temporary.path(), GenerationStoreOptions::default()),
        Err(GenerationStoreError::Symlink { .. })
    ));
    assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"keep");
}

#[test]
fn logical_generation_id_is_independent_of_physical_layout() {
    let first_evidence = GenerationArtifactEvidence::new(
        ArtifactTreeEvidence::new(digest("search-a"), 1, 10),
        ArtifactTreeEvidence::new(digest("refs-a"), 2, 20),
        ArtifactTreeEvidence::new(digest("state-a"), 3, 30),
    );
    let second_evidence = GenerationArtifactEvidence::new(
        ArtifactTreeEvidence::new(digest("search-b"), 10, 100),
        ArtifactTreeEvidence::new(digest("refs-b"), 20, 200),
        ArtifactTreeEvidence::new(digest("state-b"), 30, 300),
    );
    let arguments = (
        WorkspaceId::from_u128(0x9001).unwrap(),
        revision("revision"),
        digest("search projection"),
        digest("reference projection"),
        digest("options"),
        digest("source state"),
    );
    let first_identity = SearchGenerationIdentityV1::new(
        arguments.0,
        arguments.1,
        GenerationProjectionDigests::new(arguments.2, arguments.3),
        Default::default(),
        arguments.4,
        arguments.5,
    )
    .unwrap();
    let second_identity = SearchGenerationIdentityV1::new(
        arguments.0,
        arguments.1,
        GenerationProjectionDigests::new(arguments.2, arguments.3),
        Default::default(),
        arguments.4,
        arguments.5,
    )
    .unwrap();
    let first = SearchGenerationManifestV1::new(first_identity, first_evidence);
    let second = SearchGenerationManifestV1::new(second_identity, second_evidence);

    assert_eq!(first.generation_id(), second.generation_id());
    assert_ne!(first.artifacts(), second.artifacts());
    assert!(!first.generation_id().directory_name().contains(':'));
    assert_eq!(
        SearchGenerationId::from_directory_name(&first.generation_id().directory_name()),
        Some(first.generation_id())
    );
    let directory_name = first.generation_id().directory_name();
    let encoded = directory_name.strip_prefix("generation-v2-").unwrap();
    let uppercase_alias = format!("generation-v2-{}", encoded.to_ascii_uppercase());
    assert_eq!(
        SearchGenerationId::from_directory_name(&uppercase_alias),
        None
    );
    let current = StoredGenerationRef::current(first.generation_id());
    let legacy =
        StoredGenerationRef::new(GenerationStorageContract::LegacyV1, first.generation_id());
    assert_eq!(
        StoredGenerationRef::from_directory_name(&current.directory_name()),
        Some(current)
    );
    assert_eq!(
        StoredGenerationRef::from_directory_name(&legacy.directory_name()),
        Some(legacy)
    );
    assert!(current.directory_name().starts_with("generation-v2-"));
    assert!(legacy.directory_name().starts_with("generation-v1-"));
    assert_ne!(current.directory_name(), legacy.directory_name());
}

#[test]
fn activation_provenance_is_not_part_of_the_generation_manifest() {
    let workspace = WorkspaceId::from_u128(0x9001).unwrap();
    let revision = revision("shared revision");
    let projections = GenerationProjectionDigests::new(
        digest("shared search projection"),
        digest("shared reference projection"),
    );
    let evidence = GenerationArtifactEvidence::new(
        ArtifactTreeEvidence::new(digest("search artifacts"), 1, 10),
        ArtifactTreeEvidence::new(digest("reference artifacts"), 1, 10),
        ArtifactTreeEvidence::new(digest("source-state artifacts"), 1, 10),
    );
    let manifest = SearchGenerationManifestV1::new(
        SearchGenerationIdentityV1::new(
            workspace,
            revision,
            projections,
            Default::default(),
            digest("options"),
            digest("source state"),
        )
        .unwrap(),
        evidence,
    );
    let encoded = serde_json::to_value(&manifest).unwrap();

    assert!(encoded.get("parent_generation").is_none());
    assert!(encoded.get("applied_transactions").is_none());
    assert!(encoded.get("transaction_receipts").is_none());
}

#[test]
fn activation_receipts_advance_without_changing_logical_generation_identity() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let current_revision = revision("stable");
    let first_change = change_set("transaction a", revision("before a"), current_revision, 1);
    let first_receipts = receipt_window(&first_change);

    let mut first_build = store.begin().unwrap();
    write_artifacts(&first_build, "stable");
    let first_manifest = manifest_for(&store, &first_build, "stable", None);
    let generation = first_manifest.generation_id();
    let first_activation = GenerationActivationEvidence::new(None, first_receipts);
    let first = store
        .prepare_publish_with_desired_revision_and_budget(
            &mut first_build,
            first_manifest,
            first_activation,
            current_revision,
            &mut AssetLoadBudget::default(),
        )
        .unwrap()
        .activate()
        .unwrap()
        .active;
    let first_ordinal = first.activation_ordinal();
    drop(first_build);

    let second_change = change_set("transaction b", revision("before b"), current_revision, 2);
    let second_receipts = first
        .transaction_receipts()
        .after_reconciled_target(
            first.manifest().workspace(),
            current_revision,
            &second_change,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut second_build = store.begin().unwrap();
    write_artifacts(&second_build, "stable");
    let second_manifest = manifest_for(&store, &second_build, "stable", Some(generation));
    assert_eq!(second_manifest.generation_id(), generation);
    let second_activation = GenerationActivationEvidence::new(Some(generation), second_receipts);
    let second = store
        .prepare_publish_with_desired_revision_and_budget(
            &mut second_build,
            second_manifest,
            second_activation,
            current_revision,
            &mut AssetLoadBudget::default(),
        )
        .unwrap()
        .activate()
        .unwrap()
        .active;

    assert_eq!(second.generation(), generation);
    assert!(second.activation_ordinal() > first_ordinal);
    assert!(matches!(
        second
            .transaction_receipts()
            .membership(&second_change, &mut AssetLoadBudget::default())
            .unwrap(),
        super::TransactionReceiptMembership::Exact
    ));

    drop(second_build);
    drop(store);
    let reopened = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let active = reopened.active().unwrap();
    assert_eq!(active.generation(), generation);
    assert_eq!(active.activation_ordinal(), second.activation_ordinal());
    assert!(matches!(
        active
            .transaction_receipts()
            .membership(&second_change, &mut AssetLoadBudget::default())
            .unwrap(),
        super::TransactionReceiptMembership::Exact
    ));
}

#[test]
fn each_persisted_semantic_identity_changes_the_generation_id() {
    let workspace = WorkspaceId::from_u128(0x9001).unwrap();
    let revision = revision("revision");
    let projections = GenerationProjectionDigests::new(digest("search"), digest("references"));
    let evidence = GenerationArtifactEvidence::new(
        ArtifactTreeEvidence::new(digest("search artifacts"), 1, 10),
        ArtifactTreeEvidence::new(digest("reference artifacts"), 1, 10),
        ArtifactTreeEvidence::new(digest("source-state artifacts"), 1, 10),
    );
    let manifest = |semantics| {
        SearchGenerationManifestV1::new(
            SearchGenerationIdentityV1::new_with_semantics(
                workspace,
                revision,
                projections,
                Default::default(),
                semantics,
                digest("options"),
                digest("source state"),
            )
            .unwrap(),
            evidence,
        )
    };
    let current = SearchSemantics::current();
    let current_id = manifest(current).generation_id();
    let next_analysis_version = current.with_analysis_version(current.analysis_version() + 1);
    assert_eq!(
        next_analysis_version.analysis_digest(),
        current.analysis_digest()
    );

    for changed in [
        next_analysis_version,
        current.with_analysis_digest(digest("analysis semantics v-next")),
        current.with_search_projection_digest(digest("search projection semantics v-next")),
        current.with_reference_projection_digest(digest("reference projection semantics v-next")),
    ] {
        assert_ne!(manifest(changed).generation_id(), current_id);
    }
}

#[test]
fn manifest_deserialization_rejects_unknown_fields_and_versions() {
    let evidence = GenerationArtifactEvidence::new(
        ArtifactTreeEvidence::new(digest("search"), 1, 1),
        ArtifactTreeEvidence::new(digest("references"), 1, 1),
        ArtifactTreeEvidence::new(digest("state"), 1, 1),
    );
    let identity = SearchGenerationIdentityV1::new(
        WorkspaceId::from_u128(1).unwrap(),
        revision("revision"),
        GenerationProjectionDigests::new(digest("search"), digest("references")),
        Default::default(),
        digest("options"),
        digest("state"),
    )
    .unwrap();
    let manifest = SearchGenerationManifestV1::new(identity, evidence);
    assert_eq!(
        serde_json::to_value(&manifest).unwrap()["contract_version"],
        SEARCH_GENERATION_STORAGE_CONTRACT_VERSION
    );

    let mut unknown = serde_json::to_value(&manifest).unwrap();
    unknown
        .as_object_mut()
        .unwrap()
        .insert("unexpected".to_owned(), serde_json::json!(true));
    assert!(serde_json::from_value::<SearchGenerationManifestV1>(unknown).is_err());

    let mut unknown_semantics = serde_json::to_value(&manifest).unwrap();
    unknown_semantics["semantics"]["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<SearchGenerationManifestV1>(unknown_semantics).is_err());

    let mut unsupported_semantics = serde_json::to_value(&manifest).unwrap();
    unsupported_semantics["semantics"]["contract_version"] = serde_json::json!(2);
    assert!(serde_json::from_value::<SearchGenerationManifestV1>(unsupported_semantics).is_err());

    let mut zero_semantic_version = serde_json::to_value(&manifest).unwrap();
    zero_semantic_version["semantics"]["analysis_version"] = serde_json::json!(0);
    assert!(serde_json::from_value::<SearchGenerationManifestV1>(zero_semantic_version).is_err());

    for unsupported_version in [
        SEARCH_GENERATION_STORAGE_CONTRACT_VERSION - 1,
        SEARCH_GENERATION_STORAGE_CONTRACT_VERSION + 1,
    ] {
        let mut unsupported = serde_json::to_value(&manifest).unwrap();
        unsupported["contract_version"] = serde_json::json!(unsupported_version);
        assert!(serde_json::from_value::<SearchGenerationManifestV1>(unsupported).is_err());
    }

    let mut invalid_summary = serde_json::to_value(&manifest).unwrap();
    invalid_summary["projection_summary"]["incomplete_assets"] = serde_json::json!(1);
    assert!(serde_json::from_value::<SearchGenerationManifestV1>(invalid_summary).is_err());

    let mut different_summary = serde_json::to_value(&manifest).unwrap();
    different_summary["projection_summary"]["search_documents"] = serde_json::json!(1);
    let different_summary =
        serde_json::from_value::<SearchGenerationManifestV1>(different_summary).unwrap();
    assert_ne!(different_summary.generation_id(), manifest.generation_id());
}

#[test]
fn reopen_ignores_incomplete_staging() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let generation = publish_generation(&mut store, "baseline", None);

    let incomplete = store.begin().unwrap();
    fs::write(incomplete.search_directory().join("partial"), b"partial").unwrap();
    drop(incomplete);
    drop(store);

    let mut reopened = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    assert_eq!(reopened.active().unwrap().generation(), generation);
    let next = publish_generation(&mut reopened, "after-gap", Some(generation));
    assert_eq!(reopened.active().unwrap().activation_ordinal(), 2);
    assert_eq!(reopened.active().unwrap().generation(), next);
}

#[test]
fn reopen_budget_is_exact_and_directory_discovery_accumulates() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let generation = publish_generation(&mut store, "baseline", None);
    drop(store);

    let mut baseline_budget = AssetLoadBudget::default();
    let baseline = GenerationStore::open(
        temporary.path(),
        GenerationStoreOptions::default(),
        &mut baseline_budget,
    )
    .unwrap();
    assert_eq!(baseline.active().unwrap().generation(), generation);
    let baseline_usage = baseline_budget.usage();
    drop(baseline);

    for ordinal in 100_u64..103 {
        fs::write(
            temporary
                .path()
                .join("activations")
                .join(format!("ignored-{ordinal:020}.json")),
            b"ignored",
        )
        .unwrap();
    }

    let mut measured_budget = AssetLoadBudget::default();
    let measured = GenerationStore::open(
        temporary.path(),
        GenerationStoreOptions::default(),
        &mut measured_budget,
    )
    .unwrap();
    assert_eq!(measured.active().unwrap().generation(), generation);
    let measured_usage = measured_budget.usage();
    drop(measured);
    assert!(measured_usage.bytes > baseline_usage.bytes);
    assert!(measured_usage.entries > baseline_usage.entries);

    let mut exact = budget_for_usage(measured_usage, measured_usage.bytes);
    let reopened = GenerationStore::open(
        temporary.path(),
        GenerationStoreOptions::default(),
        &mut exact,
    )
    .unwrap();
    assert_eq!(reopened.active().unwrap().generation(), generation);
    assert_eq!(exact.usage(), measured_usage);
    drop(reopened);

    let mut one_short = budget_for_usage(measured_usage, measured_usage.bytes - 1);
    let error = GenerationStore::open(
        temporary.path(),
        GenerationStoreOptions::default(),
        &mut one_short,
    )
    .unwrap_err();
    assert_byte_budget_error(error);
}

#[test]
fn latest_activation_contract_rejects_deep_wide_and_trailing_heads() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    publish_generation(&mut store, "baseline", None);
    drop(store);

    let activations = temporary.path().join("activations");
    let valid_activation = fs::read(
        fs::read_dir(&activations)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path(),
    )
    .unwrap();
    let mut deep = serde_json::json!(0);
    for _ in 0..4 {
        deep = serde_json::json!([deep]);
    }
    let mut trailing = valid_activation;
    trailing.extend_from_slice(b" null");
    let latest = activations.join("00000000000000000100.json");
    for invalid in [
        serde_json::to_vec(&deep).unwrap(),
        serde_json::to_vec(&vec![0_u8; 64]).unwrap(),
        trailing,
    ] {
        fs::write(&latest, invalid).unwrap();
        let error = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::ContractJson {
                artifact: "activation record",
                ..
            }
        ));
    }
}

#[test]
fn latest_manifest_contract_rejects_deep_wide_and_trailing_documents() {
    let mut deep = serde_json::json!(0);
    for _ in 0..10 {
        deep = serde_json::json!([deep]);
    }
    for case in 0..3 {
        let temporary = TempDir::new().unwrap();
        let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
        let generation = publish_generation(&mut store, "latest", None);
        let manifest_path = store.generation_directory(generation).join("manifest.json");
        let invalid = match case {
            0 => serde_json::to_vec(&deep).unwrap(),
            1 => serde_json::to_vec(&vec![0_u8; 5_000]).unwrap(),
            _ => {
                let mut trailing = fs::read(&manifest_path).unwrap();
                trailing.extend_from_slice(b" null");
                trailing
            }
        };
        drop(store);
        fs::write(manifest_path, invalid).unwrap();

        let error = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::ContractJson {
                artifact: "generation manifest",
                ..
            }
        ));
    }
}

#[test]
fn corrupted_latest_generation_fails_closed_instead_of_rolling_back_freshness() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 1,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    let first = publish_generation(&mut store, "first", None);
    let _second = publish_generation(&mut store, "second", Some(first));
    let second_search = store.active().unwrap().search_directory().join("segments");
    fs::write(second_search, b"corrupt").unwrap();
    drop(store);

    let error = open_store(temporary.path(), options).unwrap_err();
    assert!(matches!(
        error,
        GenerationStoreError::ArtifactEvidenceMismatch { .. }
    ));
}

#[test]
fn pre_commit_publish_failures_never_change_the_active_generation() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 8,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);

    for (index, failpoint) in [
        GenerationFailpoint::Search,
        GenerationFailpoint::References,
        GenerationFailpoint::SourceState,
        GenerationFailpoint::Activation,
    ]
    .into_iter()
    .enumerate()
    {
        let label = format!("candidate-{index}");
        let mut build = store.begin().unwrap();
        write_artifacts(&build, &label);
        let manifest = manifest_for(&store, &build, &label, Some(baseline));
        let error = store
            .prepare_publish_with_failpoint(&mut build, manifest, failpoint)
            .and_then(|prepared| prepared.activate())
            .unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::InjectedFailure { checkpoint }
                if checkpoint == failpoint
        ));
        build
            .abort_with_budget(&mut AssetLoadBudget::default())
            .unwrap();
        assert_eq!(store.active().unwrap().generation(), baseline);

        drop(build);
        drop(store);
        store = open_store(temporary.path(), options).unwrap();
        assert_eq!(store.active().unwrap().generation(), baseline);
    }

    let mut retry = store.begin().unwrap();
    write_artifacts(&retry, "candidate-3");
    let retry_manifest = manifest_for(&store, &retry, "candidate-3", Some(baseline));
    let repaired = store
        .prepare_publish(&mut retry, retry_manifest)
        .unwrap()
        .activate()
        .unwrap();
    assert_ne!(repaired.active.generation(), baseline);

    drop(retry);
    drop(store);
    let reopened = open_store(temporary.path(), options).unwrap();
    assert_eq!(
        reopened.active().unwrap().generation(),
        repaired.active.generation()
    );
}

#[test]
fn activation_precommit_failure_cleans_staging_and_preserves_the_active_generation() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 2,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);
    let mut build = store.begin().unwrap();
    write_artifacts(&build, "precommit-failure");
    let manifest = manifest_for(&store, &build, "precommit-failure", Some(baseline));
    let prepared = store
        .prepare_publish_with_failpoint(
            &mut build,
            manifest,
            GenerationFailpoint::ActivationPreCommit,
        )
        .unwrap();
    let activation_ordinal = prepared.snapshot().activation_ordinal();
    let staging_activation = temporary
        .path()
        .join(".staging")
        .join(activation_staging_file_name(activation_ordinal));

    assert!(matches!(
        prepared.activate(),
        Err(GenerationStoreError::InjectedFailure {
            checkpoint: GenerationFailpoint::ActivationPreCommit
        })
    ));
    assert!(!staging_activation.exists());
    assert_eq!(store.active().unwrap().generation(), baseline);

    drop(build);
    drop(store);
    let reopened = open_store(temporary.path(), options).unwrap();
    assert_eq!(reopened.active().unwrap().generation(), baseline);
}

#[test]
fn activation_directory_sync_failure_returns_committed_generation_consistent_with_reopen() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 1,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);
    let mut build = store.begin().unwrap();
    write_artifacts(&build, "sync-warning");
    let manifest = manifest_for(&store, &build, "sync-warning", Some(baseline));
    let candidate = manifest.generation_id();

    let report = store
        .prepare_publish_with_failpoint(
            &mut build,
            manifest,
            GenerationFailpoint::ActivationDirectorySync,
        )
        .unwrap()
        .activate()
        .unwrap();

    assert_eq!(report.active.generation(), candidate);
    assert_eq!(store.active().unwrap().generation(), candidate);
    assert!(report.warnings.iter().any(|warning| {
        warning.kind() == GenerationPublishWarningKind::PostCommitDurability
            && warning.message().contains("ActivationDirectorySync")
    }));

    drop(build);
    drop(store);
    let reopened = open_store(temporary.path(), options).unwrap();
    assert_eq!(reopened.active().unwrap().generation(), candidate);
}

#[test]
fn activation_cleanup_failure_returns_committed_generation_consistent_with_reopen() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 1,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);
    let mut build = store.begin().unwrap();
    write_artifacts(&build, "cleanup-warning");
    let manifest = manifest_for(&store, &build, "cleanup-warning", Some(baseline));
    let candidate = manifest.generation_id();

    let report = store
        .prepare_publish_with_failpoint(
            &mut build,
            manifest,
            GenerationFailpoint::ActivationCleanup,
        )
        .unwrap()
        .activate()
        .unwrap();
    let abandoned_staging_activation = temporary.path().join(".staging").join(format!(
        "activation-{:020}.json",
        report.active.activation_ordinal()
    ));

    assert_eq!(report.active.generation(), candidate);
    assert_eq!(store.active().unwrap().generation(), candidate);
    assert!(abandoned_staging_activation.is_file());
    assert!(report.warnings.iter().any(|warning| {
        warning.kind() == GenerationPublishWarningKind::PostCommitCleanup
            && warning.message().contains("ActivationCleanup")
    }));

    drop(build);
    drop(store);
    let reopened = open_store(temporary.path(), options).unwrap();
    assert_eq!(reopened.active().unwrap().generation(), candidate);
    assert!(!abandoned_staging_activation.exists());
}

#[test]
fn prepared_generation_is_readable_before_activation_and_reusable_if_dropped() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);
    let mut build = store.begin().unwrap();
    write_artifacts(&build, "candidate");
    let manifest = manifest_for(&store, &build, "candidate", Some(baseline));
    let candidate = manifest.generation_id();

    let prepared = store.prepare_publish(&mut build, manifest).unwrap();
    assert_eq!(
        fs::read(prepared.snapshot().search_directory().join("segments")).unwrap(),
        b"search:candidate"
    );
    drop(prepared);
    assert_eq!(store.active().unwrap().generation(), baseline);

    let mut retry = store.begin().unwrap();
    write_artifacts(&retry, "candidate");
    let retry_manifest = manifest_for(&store, &retry, "candidate", Some(baseline));
    let prepared = store.prepare_publish(&mut retry, retry_manifest).unwrap();
    assert_eq!(prepared.snapshot().generation(), candidate);
    let report = prepared.activate().unwrap();
    assert_eq!(report.active.generation(), candidate);
}

#[test]
fn publish_rejects_source_state_that_does_not_match_the_manifest() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);
    let mut build = store.begin().unwrap();
    write_artifacts(&build, "candidate");
    let build_path = build.directory().to_path_buf();
    let evidence = store.measure_artifacts(&build).unwrap();
    let identity = SearchGenerationIdentityV1::new(
        WorkspaceId::from_u128(0x9001).unwrap(),
        revision("candidate"),
        GenerationProjectionDigests::new(
            digest("search-projection:candidate"),
            digest("reference-projection:candidate"),
        ),
        Default::default(),
        digest("options"),
        digest("wrong-source-state"),
    )
    .unwrap();

    assert!(matches!(
        store.prepare_publish(
            &mut build,
            SearchGenerationManifestV1::new(identity, evidence),
        ),
        Err(GenerationStoreError::SourceState { .. })
    ));
    build
        .abort_with_budget(&mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(store.active().unwrap().generation(), baseline);
    assert!(!build_path.exists());
}

#[test]
fn corrupt_completed_orphan_is_replaced_by_the_same_logical_generation() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 2,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);

    let mut build = store.begin().unwrap();
    write_artifacts(&build, "candidate");
    let manifest = manifest_for(&store, &build, "candidate", Some(baseline));
    let generation = manifest.generation_id();
    assert!(matches!(
        store
            .prepare_publish_with_failpoint(&mut build, manifest, GenerationFailpoint::Activation,)
            .and_then(|prepared| prepared.activate()),
        Err(GenerationStoreError::InjectedFailure {
            checkpoint: GenerationFailpoint::Activation
        })
    ));
    build
        .abort_with_budget(&mut AssetLoadBudget::default())
        .unwrap();
    fs::write(
        store
            .generation_directory(generation)
            .join("search")
            .join("segments"),
        b"corrupt",
    )
    .unwrap();

    let mut retry = store.begin().unwrap();
    write_artifacts(&retry, "candidate");
    let retry_manifest = manifest_for(&store, &retry, "candidate", Some(baseline));
    let report = store
        .prepare_publish(&mut retry, retry_manifest)
        .unwrap()
        .activate()
        .unwrap();

    assert_eq!(report.active.generation(), generation);
    assert_eq!(store.active().unwrap().generation(), generation);
    assert!(
        fs::read_dir(temporary.path().join(".staging"))
            .unwrap()
            .all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("quarantine-")
            })
    );
}

#[test]
fn retention_keeps_active_and_configured_previous_generations() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 1,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    let first = publish_generation(&mut store, "first", None);
    let second = publish_generation(&mut store, "second", Some(first));
    let third = publish_generation(&mut store, "third", Some(second));

    assert!(!store.generation_directory(first).exists());
    assert!(store.generation_directory(second).is_dir());
    assert!(store.generation_directory(third).is_dir());
    assert_eq!(store.active().unwrap().generation(), third);

    drop(store);
    let reopened = open_store(temporary.path(), options).unwrap();
    assert_eq!(reopened.active().unwrap().generation(), third);
}

#[test]
fn retention_tracks_legacy_and_current_directories_as_distinct_generations() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 1,
    };
    drop(open_store(temporary.path(), options).unwrap());
    let legacy =
        install_frozen_legacy_generation(temporary.path(), 3, revision("legacy-retention"));
    let legacy_ref =
        StoredGenerationRef::new(GenerationStorageContract::LegacyV1, legacy.generation);
    let mut store = open_store(temporary.path(), options).unwrap();

    let first_current = publish_generation_for_workspace(
        &mut store,
        "first-current",
        Some(legacy.generation),
        legacy.workspace,
    );
    assert!(
        temporary
            .path()
            .join(super::GENERATIONS_DIRECTORY)
            .join(legacy_ref.directory_name())
            .is_dir()
    );
    assert!(store.generation_directory(first_current).is_dir());

    let second_current = publish_generation_for_workspace(
        &mut store,
        "second-current",
        Some(first_current),
        legacy.workspace,
    );
    assert!(
        !temporary
            .path()
            .join(super::GENERATIONS_DIRECTORY)
            .join(legacy_ref.directory_name())
            .exists()
    );
    assert!(store.generation_directory(first_current).is_dir());
    assert!(store.generation_directory(second_current).is_dir());
}

#[test]
fn retention_bounds_activation_history_with_generation_history() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 1,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    let mut parent = None;
    for label in ["one", "two", "three", "four", "five"] {
        parent = Some(publish_generation(&mut store, label, parent));
    }

    assert_eq!(
        fs::read_dir(temporary.path().join("generations"))
            .unwrap()
            .count(),
        2
    );
    assert_eq!(
        fs::read_dir(temporary.path().join("activations"))
            .unwrap()
            .count(),
        2
    );
}

#[test]
fn disk_estimate_accounts_for_old_and_new_generations() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 0,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    publish_generation(&mut store, "baseline", None);

    let estimate = store
        .estimate_publish(4_096, &mut AssetLoadBudget::default())
        .unwrap();
    assert!(estimate.old_active_generation_bytes > 0);
    assert_eq!(
        estimate.publish_peak_bytes,
        estimate.existing_generation_bytes + estimate.new_generation_bytes
    );
    assert_eq!(estimate.retained_bytes_after_publish, 4_096);
    assert_eq!(
        estimate.reclaimable_bytes_after_publish,
        estimate.existing_generation_bytes
    );
}

#[cfg(unix)]
#[test]
fn store_rejects_symbolic_links_in_managed_directories() {
    use std::os::unix::fs::symlink;

    let temporary = TempDir::new().unwrap();
    let store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    drop(store);
    let target = temporary.path().join("target.json");
    fs::write(&target, b"{}").unwrap();
    symlink(
        &target,
        temporary
            .path()
            .join("activations")
            .join("00000000000000000001.json"),
    )
    .unwrap();

    assert!(matches!(
        open_store(temporary.path(), GenerationStoreOptions::default()),
        Err(GenerationStoreError::Symlink { .. })
    ));
}

#[cfg(unix)]
#[test]
fn publish_syncs_read_only_artifacts_before_activation() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let mut build = store.begin().unwrap();
    write_artifacts(&build, "read-only");
    let artifact = build.search_directory().join("segments");
    let mut permissions = fs::metadata(&artifact).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&artifact, permissions).unwrap();
    let manifest = manifest_for(&store, &build, "read-only", None);

    let report = store
        .prepare_publish(&mut build, manifest)
        .unwrap()
        .activate()
        .unwrap();
    assert_eq!(
        fs::read(report.active.search_directory().join("segments")).unwrap(),
        b"search:read-only"
    );
}

#[cfg(windows)]
#[test]
fn store_rejects_windows_junctions_in_managed_directories() {
    let temporary = TempDir::new().unwrap();
    let store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    drop(store);
    let target = temporary.path().join("junction-target");
    let junction = temporary.path().join("activations").join("junction");
    fs::create_dir(&target).unwrap();
    create_junction(&junction, &target);

    let reopened = open_store(temporary.path(), GenerationStoreOptions::default());
    fs::remove_dir(&junction).unwrap();
    assert!(
        matches!(&reopened, Err(GenerationStoreError::ReparsePoint { .. })),
        "unexpected junction reopen result: {reopened:?}"
    );
}

#[cfg(windows)]
#[test]
fn artifact_measurement_rejects_nested_windows_junctions() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let build = store.begin().unwrap();
    let target = temporary.path().join("artifact-junction-target");
    let junction = build.search_directory().join("junction");
    fs::create_dir(&target).unwrap();
    create_junction(&junction, &target);

    let measured = store.measure_artifacts(&build);
    assert!(
        matches!(&measured, Err(GenerationStoreError::ReparsePoint { .. })),
        "unexpected junction measurement result: {measured:?}"
    );

    fs::remove_dir(&junction).unwrap();
    drop(build);
}
