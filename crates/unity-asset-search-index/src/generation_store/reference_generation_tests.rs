use std::fs;
use std::path::{Path, PathBuf};

use super::source_state::{SOURCE_STATE_CONTRACT_VERSION, SOURCE_STATE_LOGICAL_IDENTITY_VERSION};
use super::{
    GenerationActivationEvidence, GenerationBuild, GenerationFailpoint,
    GenerationPublishWarningKind, GenerationStartupDisposition, GenerationStore,
    GenerationStoreError, GenerationStoreOptions, IndexRebuildReason, SOURCE_STATE_FILE,
    TransactionReceiptWindow, activation_file_name, activation_pending_file_name,
    activation_recovery_file_name, legacy_activation_staging_file_name, quarantine_directory_name,
    staging_directory_name,
};
use crate::generation::{
    ArtifactTreeEvidence, GenerationArtifactEvidence, GenerationProjectionDigests,
    LEGACY_COUPLED_GENERATION_STORAGE_CONTRACT_VERSION,
    LEGACY_SOURCE_STATE_V4_STORAGE_CONTRACT_VERSION, SEARCH_GENERATION_MANIFEST_CONTRACT_VERSION,
    SEARCH_GENERATION_STORAGE_CONTRACT_VERSION, SearchGenerationId, SearchGenerationIdentityV1,
    SearchGenerationManifestV1,
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

fn open_store_with_startup_disposition(
    root: impl AsRef<Path>,
    options: GenerationStoreOptions,
) -> Result<super::OpenedGenerationStore, GenerationStoreError> {
    let root = super::initialize_root(root.as_ref())?;
    GenerationStore::open_at_root(
        root,
        super::GenerationStoreRootAuthority::Fixture,
        options,
        &mut AssetLoadBudget::default(),
        None,
    )
}

fn open_store_with_startup_failpoint(
    root: impl AsRef<Path>,
    options: GenerationStoreOptions,
    failpoint: GenerationFailpoint,
) -> Result<super::OpenedGenerationStore, GenerationStoreError> {
    let root = super::initialize_root(root.as_ref())?;
    GenerationStore::open_at_root(
        root,
        super::GenerationStoreRootAuthority::Fixture,
        options,
        &mut AssetLoadBudget::default(),
        Some(failpoint),
    )
}

fn write_activation_record(
    root: &Path,
    ordinal: u64,
    contract_version: u16,
    storage_contract: Option<u16>,
) -> (SearchGenerationId, PathBuf) {
    let generation = SearchGenerationId::new(digest(&format!("generation-{ordinal}")));
    let workspace = WorkspaceId::from_u128(0x9001).unwrap();
    let revision = revision(&format!("revision-{ordinal}"));
    let mut record = serde_json::json!({
        "contract_version": contract_version,
        "ordinal": ordinal,
        "generation": generation,
        "manifest_digest": digest(&format!("manifest-{ordinal}")),
        "workspace": workspace,
        "revision": revision,
    });
    if contract_version >= super::REVISIONED_ACTIVATION_CONTRACT_VERSION {
        record["desired_revision"] = serde_json::to_value(revision).unwrap();
    }
    if contract_version == super::GENERATION_HEAD_CONTRACT_VERSION {
        record["generation_storage_contract"] =
            serde_json::json!(storage_contract.expect("v3 activation requires storage"));
        record["transaction_receipts"] =
            serde_json::to_value(TransactionReceiptWindow::empty()).unwrap();
    }
    let path = root
        .join(super::ACTIVATIONS_DIRECTORY)
        .join(activation_file_name(ordinal));
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    (generation, path)
}

fn assert_rebuild_required(
    disposition: GenerationStartupDisposition,
    reason: IndexRebuildReason,
    activation_ordinal: u64,
    generation: SearchGenerationId,
) {
    let GenerationStartupDisposition::RebuildRequired(required) = disposition else {
        panic!("expected a rebuild-required startup disposition");
    };
    let expected_revision = revision(&format!("revision-{activation_ordinal}"));
    assert_eq!(required.reason, reason);
    assert_eq!(required.activation_ordinal, activation_ordinal);
    assert_eq!(required.generation, generation);
    assert_eq!(
        required.bootstrap.workspace(),
        WorkspaceId::from_u128(0x9001).unwrap()
    );
    assert_eq!(required.bootstrap.actual_revision(), expected_revision);
    assert_eq!(required.bootstrap.desired_revision(), expected_revision);
    assert!(
        required
            .bootstrap
            .transaction_receipts()
            .as_slice()
            .is_empty()
    );
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
        build.source_state_directory().join(SOURCE_STATE_FILE),
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
fn unsynced_desired_revision_head_recovers_after_the_final_link_is_lost() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    let mut store = open_store(temporary.path(), options).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);
    let desired = revision("desired");

    let warnings = store
        .record_desired_revision_with_failpoint(
            desired,
            &mut AssetLoadBudget::default(),
            Some(GenerationFailpoint::ActivationDirectorySync),
        )
        .unwrap();
    assert!(warnings.iter().any(|warning| {
        warning.kind() == GenerationPublishWarningKind::PostCommitDurability
            && warning.message().contains("ActivationDirectorySync")
    }));

    let ordinal = store.active().unwrap().activation_ordinal();
    let activation = temporary
        .path()
        .join(super::ACTIVATIONS_DIRECTORY)
        .join(activation_file_name(ordinal));
    let recovery = temporary
        .path()
        .join(super::STAGING_DIRECTORY)
        .join(activation_recovery_file_name(ordinal));
    assert!(activation.is_file());
    assert!(recovery.is_file());

    drop(store);
    fs::remove_file(&activation).unwrap();

    let reopened = open_store(temporary.path(), options).unwrap();
    let active = reopened.active().unwrap();
    assert_eq!(active.generation(), baseline);
    assert_eq!(active.manifest().revision(), revision("baseline"));
    assert_eq!(active.desired_revision(), desired);
    assert!(!recovery.exists());
}

#[test]
fn uncertain_activation_rollback_requires_reopen_before_same_process_recovery() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    let mut store = open_store(temporary.path(), options).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);
    let baseline_ordinal = store.active().unwrap().activation_ordinal();
    let desired = revision("desired");

    let error = store
        .record_desired_revision_with_failpoint(
            desired,
            &mut AssetLoadBudget::default(),
            Some(GenerationFailpoint::ActivationRecoverySyncAndRollbackCleanup),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        GenerationStoreError::ActivationRollbackFailed {
            primary,
            cleanup,
        } if matches!(
            *primary,
            GenerationStoreError::InjectedFailure {
                checkpoint: GenerationFailpoint::ActivationRecoveryDirectorySync,
            }
        ) && matches!(
            *cleanup,
            GenerationStoreError::InjectedFailure {
                checkpoint: GenerationFailpoint::ActivationRollbackCleanup,
            }
        )
    ));
    assert_eq!(
        store.active().unwrap().desired_revision(),
        revision("baseline")
    );

    let uncertain_ordinal = baseline_ordinal + 1;
    let activation = temporary
        .path()
        .join(super::ACTIVATIONS_DIRECTORY)
        .join(activation_file_name(uncertain_ordinal));
    let pending = temporary
        .path()
        .join(super::STAGING_DIRECTORY)
        .join(activation_pending_file_name(uncertain_ordinal));
    let recovery = temporary
        .path()
        .join(super::STAGING_DIRECTORY)
        .join(activation_recovery_file_name(uncertain_ordinal));
    assert!(activation.is_file());
    assert!(pending.is_file());
    assert!(recovery.is_file());

    assert!(matches!(
        store.reconcile_abandoned_staging(&mut AssetLoadBudget::default()),
        Err(GenerationStoreError::ActivationOutcomeUnknown)
    ));
    assert!(matches!(
        store.begin(),
        Err(GenerationStoreError::ActivationOutcomeUnknown)
    ));
    assert_eq!(store.active().unwrap().generation(), baseline);

    drop(store);
    let reopened = open_store(temporary.path(), options).unwrap();
    let active = reopened.active().unwrap();
    assert_eq!(active.generation(), baseline);
    assert_eq!(active.manifest().revision(), revision("baseline"));
    assert_eq!(active.desired_revision(), desired);
    assert!(!pending.exists());
    assert!(!recovery.exists());
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
fn obsolete_activation_contracts_remain_as_durable_rebuild_heads() {
    for contract_version in [
        super::LEGACY_ACTIVATION_CONTRACT_VERSION,
        super::REVISIONED_ACTIVATION_CONTRACT_VERSION,
    ] {
        let temporary = TempDir::new().unwrap();
        let options = GenerationStoreOptions::default();
        drop(open_store(temporary.path(), options).unwrap());
        let (generation, activation_path) =
            write_activation_record(temporary.path(), 1, contract_version, None);
        let obsolete_generation =
            temporary
                .path()
                .join(super::GENERATIONS_DIRECTORY)
                .join(format!(
                    "generation-v1-{}",
                    hex::encode(generation.digest().as_bytes())
                ));
        fs::create_dir(&obsolete_generation).unwrap();
        fs::write(obsolete_generation.join("sentinel"), b"obsolete").unwrap();

        let opened = open_store_with_startup_disposition(temporary.path(), options).unwrap();
        let (store, recovery, disposition) = opened.into_parts();
        assert_eq!(recovery.unwrap().removed_entries(), 1);
        assert_eq!(store.active(), None);
        assert_rebuild_required(
            disposition,
            IndexRebuildReason::ObsoleteActivationContract {
                actual: contract_version,
            },
            1,
            generation,
        );
        assert!(!obsolete_generation.exists());
        assert_eq!(
            fs::read_dir(temporary.path().join(super::ACTIVATIONS_DIRECTORY))
                .unwrap()
                .count(),
            1
        );
        assert!(activation_path.is_file());
    }
}

#[test]
fn activation_v2_preserves_a_distinct_desired_revision_in_the_rebuild_bootstrap() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    drop(open_store(temporary.path(), options).unwrap());
    let (generation, activation_path) = write_activation_record(
        temporary.path(),
        1,
        super::REVISIONED_ACTIVATION_CONTRACT_VERSION,
        None,
    );
    let desired = revision("desired-after-rebuild");
    let mut activation: serde_json::Value =
        serde_json::from_slice(&fs::read(&activation_path).unwrap()).unwrap();
    activation["desired_revision"] = serde_json::to_value(desired).unwrap();
    fs::write(&activation_path, serde_json::to_vec(&activation).unwrap()).unwrap();

    let opened = open_store_with_startup_disposition(temporary.path(), options).unwrap();
    let (_, recovery, disposition) = opened.into_parts();
    assert_eq!(recovery.unwrap().removed_entries(), 0);
    let GenerationStartupDisposition::RebuildRequired(required) = disposition else {
        panic!("expected a rebuild-required startup disposition");
    };
    assert_eq!(required.generation, generation);
    assert_eq!(required.bootstrap.actual_revision(), revision("revision-1"));
    assert_eq!(required.bootstrap.desired_revision(), desired);
}

#[test]
fn obsolete_activation_contracts_require_exact_wire_before_rebuild() {
    let cases = [
        (
            super::LEGACY_ACTIVATION_CONTRACT_VERSION,
            "desired_revision",
            Some(serde_json::to_value(revision("unexpected-desired")).unwrap()),
        ),
        (
            super::REVISIONED_ACTIVATION_CONTRACT_VERSION,
            "desired_revision",
            None,
        ),
        (
            super::REVISIONED_ACTIVATION_CONTRACT_VERSION,
            "desired_revision",
            Some(serde_json::Value::Null),
        ),
    ];

    for (contract_version, field, replacement) in cases {
        let temporary = TempDir::new().unwrap();
        let options = GenerationStoreOptions::default();
        drop(open_store(temporary.path(), options).unwrap());
        let (_, activation_path) =
            write_activation_record(temporary.path(), 1, contract_version, None);
        let mut activation: serde_json::Value =
            serde_json::from_slice(&fs::read(&activation_path).unwrap()).unwrap();
        match replacement {
            Some(value) => activation[field] = value,
            None => {
                activation.as_object_mut().unwrap().remove(field);
            }
        }
        fs::write(&activation_path, serde_json::to_vec(&activation).unwrap()).unwrap();

        let error = open_store_with_startup_disposition(temporary.path(), options).unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::InvalidGenerationHead { .. }
        ));
        assert!(activation_path.is_file());
    }
}

#[test]
fn obsolete_activation_filename_ordinal_must_match_the_wire_record() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    drop(open_store(temporary.path(), options).unwrap());
    let (_, activation_path) = write_activation_record(
        temporary.path(),
        1,
        super::REVISIONED_ACTIVATION_CONTRACT_VERSION,
        None,
    );
    let mut activation: serde_json::Value =
        serde_json::from_slice(&fs::read(&activation_path).unwrap()).unwrap();
    activation["ordinal"] = serde_json::json!(2);
    fs::write(&activation_path, serde_json::to_vec(&activation).unwrap()).unwrap();

    let error = open_store_with_startup_disposition(temporary.path(), options).unwrap_err();
    assert!(matches!(
        error,
        GenerationStoreError::ActivationOrdinalMismatch {
            expected: 1,
            actual: 2,
            ..
        }
    ));
    assert!(activation_path.is_file());
}

#[test]
fn storage_v1_activation_requires_rebuild_without_parsing_legacy_generation_bytes() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    drop(open_store(temporary.path(), options).unwrap());
    let (generation, _) = write_activation_record(
        temporary.path(),
        1,
        super::GENERATION_HEAD_CONTRACT_VERSION,
        Some(1),
    );
    let obsolete_generation = temporary
        .path()
        .join(super::GENERATIONS_DIRECTORY)
        .join(format!(
            "generation-v1-{}",
            hex::encode(generation.digest().as_bytes())
        ));
    fs::create_dir(&obsolete_generation).unwrap();
    fs::write(obsolete_generation.join("malformed"), b"not a generation").unwrap();

    let opened = open_store_with_startup_disposition(temporary.path(), options).unwrap();
    let (store, recovery, disposition) = opened.into_parts();
    assert_eq!(recovery.unwrap().removed_entries(), 1);
    assert_eq!(store.active(), None);
    assert_rebuild_required(
        disposition,
        IndexRebuildReason::ObsoleteGenerationStorage { actual: 1 },
        1,
        generation,
    );
    assert!(!obsolete_generation.exists());
}

#[test]
fn storage_v3_keeps_a_valid_projection_queryable_until_current_rebuild_commits() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    let mut store = open_store(temporary.path(), options).unwrap();
    let generation = publish_generation(&mut store, "legacy-projection", None);
    let current_directory = store.active().unwrap().directory().to_path_buf();
    let current_activation = temporary
        .path()
        .join(super::ACTIVATIONS_DIRECTORY)
        .join(super::activation_file_name(1));
    drop(store);

    let legacy_directory = super::rewrite_generation_fixture_as_opaque_storage(
        &current_directory,
        &current_activation,
        generation,
        LEGACY_SOURCE_STATE_V4_STORAGE_CONTRACT_VERSION,
        b"must not be parsed",
    );

    let opened = open_store_with_startup_disposition(temporary.path(), options).unwrap();
    let (mut store, recovery, disposition) = opened.into_parts();
    assert_eq!(recovery.unwrap().removed_entries(), 0);
    assert_eq!(store.active().unwrap().directory(), legacy_directory);
    let GenerationStartupDisposition::RebuildRequired(required) = disposition else {
        panic!("expected a rebuild-required startup disposition");
    };
    assert_eq!(
        required.reason,
        IndexRebuildReason::ObsoleteGenerationStorage {
            actual: LEGACY_SOURCE_STATE_V4_STORAGE_CONTRACT_VERSION,
        }
    );
    assert_eq!(required.activation_ordinal, 1);
    assert_eq!(required.generation, generation);
    assert_eq!(
        required.bootstrap.actual_revision(),
        revision("legacy-projection")
    );

    let rebuilt = publish_generation(&mut store, "rebuilt-projection", Some(generation));
    assert_ne!(rebuilt, generation);
    assert!(!legacy_directory.exists());
}

#[test]
fn storage_v3_source_state_evidence_mismatch_fails_closed() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    let mut store = open_store(temporary.path(), options).unwrap();
    let generation = publish_generation(&mut store, "legacy-corruption", None);
    let current_directory = store.active().unwrap().directory().to_path_buf();
    let activation = temporary
        .path()
        .join(super::ACTIVATIONS_DIRECTORY)
        .join(super::activation_file_name(1));
    drop(store);

    let legacy_directory = super::rewrite_generation_fixture_as_opaque_storage(
        &current_directory,
        &activation,
        generation,
        LEGACY_SOURCE_STATE_V4_STORAGE_CONTRACT_VERSION,
        b"attested opaque state",
    );
    fs::write(
        legacy_directory
            .join(super::SOURCE_STATE_ARTIFACT_DIRECTORY)
            .join("source-state-v4.json"),
        b"tampered opaque state",
    )
    .unwrap();

    let error = open_store_with_startup_disposition(temporary.path(), options).unwrap_err();
    assert!(matches!(
        error,
        GenerationStoreError::ArtifactEvidenceMismatch { .. }
    ));
    assert!(legacy_directory.is_dir());
    assert!(activation.is_file());
}

#[test]
fn desired_revision_on_storage_v3_preserves_the_obsolete_storage_contract() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    let mut store = open_store(temporary.path(), options).unwrap();
    let generation = publish_generation(&mut store, "legacy-desired", None);
    let current_directory = store.active().unwrap().directory().to_path_buf();
    let activation = temporary
        .path()
        .join(super::ACTIVATIONS_DIRECTORY)
        .join(super::activation_file_name(1));
    drop(store);

    super::rewrite_generation_fixture_as_opaque_storage(
        &current_directory,
        &activation,
        generation,
        LEGACY_SOURCE_STATE_V4_STORAGE_CONTRACT_VERSION,
        b"attested opaque state",
    );
    let opened = open_store_with_startup_disposition(temporary.path(), options).unwrap();
    let (mut store, recovery, disposition) = opened.into_parts();
    recovery.unwrap();
    assert!(matches!(
        disposition,
        GenerationStartupDisposition::RebuildRequired(_)
    ));

    let desired = revision("legacy-desired-next");
    store
        .record_desired_revision(desired, &mut AssetLoadBudget::default())
        .unwrap();
    let active = store.active().unwrap();
    assert_eq!(active.desired_revision(), desired);
    assert_eq!(
        active.storage_contract(),
        LEGACY_SOURCE_STATE_V4_STORAGE_CONTRACT_VERSION
    );
    let latest_activation = temporary
        .path()
        .join(super::ACTIVATIONS_DIRECTORY)
        .join(super::activation_file_name(active.activation_ordinal()));
    let activation: serde_json::Value =
        serde_json::from_slice(&fs::read(latest_activation).unwrap()).unwrap();
    assert_eq!(
        activation["generation_storage_contract"],
        serde_json::json!(LEGACY_SOURCE_STATE_V4_STORAGE_CONTRACT_VERSION)
    );
    drop(store);

    let reopened = open_store_with_startup_disposition(temporary.path(), options).unwrap();
    let (store, recovery, disposition) = reopened.into_parts();
    recovery.unwrap();
    assert_eq!(store.active().unwrap().desired_revision(), desired);
    let GenerationStartupDisposition::RebuildRequired(required) = disposition else {
        panic!("expected storage migration to remain rebuild-required");
    };
    assert_eq!(required.bootstrap().desired_revision(), desired);
}

#[test]
fn future_storage_contract_fails_closed_and_preserves_its_activation() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    drop(open_store(temporary.path(), options).unwrap());
    let (_, activation) = write_activation_record(
        temporary.path(),
        1,
        super::GENERATION_HEAD_CONTRACT_VERSION,
        Some(SEARCH_GENERATION_STORAGE_CONTRACT_VERSION + 1),
    );

    let error = open_store_with_startup_disposition(temporary.path(), options).unwrap_err();
    assert!(matches!(
        error,
        GenerationStoreError::UnsupportedVersion {
            artifact: "generation storage",
            actual,
            expected: SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
        } if actual == SEARCH_GENERATION_STORAGE_CONTRACT_VERSION + 1
    ));
    assert!(activation.is_file());
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
fn failed_startup_cleanup_advances_past_abandoned_build_ordinals() {
    let temporary = TempDir::new().unwrap();
    drop(open_store(temporary.path(), GenerationStoreOptions::default()).unwrap());
    let abandoned = temporary
        .path()
        .join(super::STAGING_DIRECTORY)
        .join(staging_directory_name(99));
    fs::create_dir(&abandoned).unwrap();

    let opened = open_store_with_startup_failpoint(
        temporary.path(),
        GenerationStoreOptions::default(),
        GenerationFailpoint::StartupStagingCleanup,
    )
    .unwrap();
    let (mut store, recovery, disposition) = opened.into_parts();
    assert!(matches!(
        recovery,
        Err(GenerationStoreError::InjectedFailure {
            checkpoint: GenerationFailpoint::StartupStagingCleanup
        })
    ));
    assert_eq!(disposition, GenerationStartupDisposition::Ready);

    let build = store.begin().unwrap();
    assert_eq!(
        build.directory().file_name().unwrap(),
        staging_directory_name(100).as_str()
    );
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
    let abandoned_activation = staging.join(activation_pending_file_name(93));
    let legacy_activation = staging.join(legacy_activation_staging_file_name(94));
    fs::create_dir(&abandoned_build).unwrap();
    fs::write(abandoned_build.join("partial"), b"partial").unwrap();
    fs::create_dir(&abandoned_quarantine).unwrap();
    fs::write(abandoned_quarantine.join("old"), b"old").unwrap();
    fs::write(&abandoned_activation, b"partial activation").unwrap();
    fs::write(&legacy_activation, b"legacy partial activation").unwrap();

    let live_build = store.begin().unwrap();
    assert!(matches!(
        store.reconcile_abandoned_staging(&mut AssetLoadBudget::default()),
        Err(GenerationStoreError::BuildAlreadyActive)
    ));
    drop(live_build);

    let report = store
        .reconcile_abandoned_staging(&mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(report.removed_entries(), 4);
    assert!(!abandoned_build.exists());
    assert!(!abandoned_quarantine.exists());
    assert!(!abandoned_activation.exists());
    assert!(!legacy_activation.exists());
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
    let prefix = format!("generation-v{SEARCH_GENERATION_STORAGE_CONTRACT_VERSION}-");
    let encoded = directory_name.strip_prefix(&prefix).unwrap();
    let uppercase_alias = format!("{prefix}{}", encoded.to_ascii_uppercase());
    assert_eq!(
        SearchGenerationId::from_directory_name(&uppercase_alias),
        None
    );
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
        SEARCH_GENERATION_MANIFEST_CONTRACT_VERSION
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
        SEARCH_GENERATION_MANIFEST_CONTRACT_VERSION - 1,
        SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
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
        .join(activation_recovery_file_name(activation_ordinal));
    let pending_activation = temporary
        .path()
        .join(".staging")
        .join(activation_pending_file_name(activation_ordinal));

    assert!(matches!(
        prepared.activate(),
        Err(GenerationStoreError::InjectedFailure {
            checkpoint: GenerationFailpoint::ActivationPreCommit
        })
    ));
    assert!(!staging_activation.exists());
    assert!(!pending_activation.exists());
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
    let pending =
        temporary
            .path()
            .join(super::STAGING_DIRECTORY)
            .join(activation_pending_file_name(
                report.active.activation_ordinal(),
            ));
    let recovery =
        temporary
            .path()
            .join(super::STAGING_DIRECTORY)
            .join(activation_recovery_file_name(
                report.active.activation_ordinal(),
            ));

    assert_eq!(report.active.generation(), candidate);
    assert_eq!(store.active().unwrap().generation(), candidate);
    assert!(report.warnings.iter().any(|warning| {
        warning.kind() == GenerationPublishWarningKind::PostCommitDurability
            && warning.message().contains("ActivationDirectorySync")
    }));
    assert!(pending.is_file());
    assert!(recovery.is_file());

    drop(build);
    drop(store);
    let reopened = open_store(temporary.path(), options).unwrap();
    assert_eq!(reopened.active().unwrap().generation(), candidate);
    assert!(!pending.exists());
    assert!(!recovery.exists());
}

#[test]
fn already_active_retry_confirms_durability_before_pruning_old_heads() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 0,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);

    let mut first = store.begin().unwrap();
    write_artifacts(&first, "candidate");
    let manifest = manifest_for(&store, &first, "candidate", Some(baseline));
    let first_report = store
        .prepare_publish_with_failpoint(
            &mut first,
            manifest,
            GenerationFailpoint::ActivationDirectorySync,
        )
        .unwrap()
        .activate()
        .unwrap();
    let candidate = first_report.active.generation();
    drop(first);

    let mut refreshed = store.begin().unwrap();
    write_artifacts(&refreshed, "candidate");
    let manifest = manifest_for(&store, &refreshed, "candidate", Some(candidate));
    store
        .prepare_publish_with_failpoint(
            &mut refreshed,
            manifest,
            GenerationFailpoint::ActivationDirectorySync,
        )
        .unwrap()
        .activate()
        .unwrap();
    drop(refreshed);

    let active_ordinal = store.active().unwrap().activation_ordinal();
    assert_eq!(
        fs::read_dir(temporary.path().join(super::ACTIVATIONS_DIRECTORY))
            .unwrap()
            .count(),
        3
    );

    let mut retry = store.begin().unwrap();
    write_artifacts(&retry, "candidate");
    let manifest = manifest_for(&store, &retry, "candidate", Some(candidate));
    let prepared = store.prepare_publish(&mut retry, manifest).unwrap();
    assert_eq!(prepared.snapshot().activation_ordinal(), active_ordinal);
    prepared.activate().unwrap();

    assert_eq!(
        fs::read_dir(temporary.path().join(super::ACTIVATIONS_DIRECTORY))
            .unwrap()
            .count(),
        1
    );
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
    let abandoned_staging_activation =
        temporary
            .path()
            .join(".staging")
            .join(activation_recovery_file_name(
                report.active.activation_ordinal(),
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
fn staging_recovery_requires_a_successful_directory_sync_before_reporting_clean() {
    let temporary = TempDir::new().unwrap();
    let staging = temporary.path().join(super::STAGING_DIRECTORY);
    let activations = temporary.path().join(super::ACTIVATIONS_DIRECTORY);
    fs::create_dir(&staging).unwrap();
    fs::create_dir(&activations).unwrap();
    let residue = staging.join(activation_pending_file_name(7));
    fs::write(&residue, b"staging residue").unwrap();

    let error = super::recover_owned_staging(
        &staging,
        &activations,
        &mut AssetLoadBudget::default(),
        Some(GenerationFailpoint::StartupStagingDirectorySync),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        GenerationStoreError::InjectedFailure {
            checkpoint: GenerationFailpoint::StartupStagingDirectorySync
        }
    ));
    assert!(!residue.exists());

    let recovered = super::recover_owned_staging(
        &staging,
        &activations,
        &mut AssetLoadBudget::default(),
        None,
    )
    .unwrap();
    assert_eq!(recovered.removed_entries(), 0);
}

#[test]
fn generation_recovery_requires_a_successful_directory_sync_before_reporting_clean() {
    let temporary = TempDir::new().unwrap();
    let generations = temporary.path().join(super::GENERATIONS_DIRECTORY);
    fs::create_dir(&generations).unwrap();
    let residue = generations.join(format!("generation-v1-{}", "0".repeat(64)));
    fs::create_dir(&residue).unwrap();

    let error = super::recover_unreferenced_generation_directories(
        &generations,
        false,
        None,
        &mut AssetLoadBudget::default(),
        Some(GenerationFailpoint::StartupGenerationDirectorySync),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        GenerationStoreError::InjectedFailure {
            checkpoint: GenerationFailpoint::StartupGenerationDirectorySync
        }
    ));
    assert!(!residue.exists());

    let recovered = super::recover_unreferenced_generation_directories(
        &generations,
        false,
        None,
        &mut AssetLoadBudget::default(),
        None,
    )
    .unwrap();
    assert_eq!(recovered.removed_entries(), 0);
}

#[test]
fn full_activation_capacity_compacts_before_rejecting_the_next_commit() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 0,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    publish_generation(&mut store, "baseline", None);
    let active_ordinal = store.active().unwrap().activation_ordinal();
    let activations = temporary.path().join(super::ACTIVATIONS_DIRECTORY);
    for ordinal in [active_ordinal + 10, active_ordinal + 11] {
        fs::write(activations.join(activation_file_name(ordinal)), b"{}").unwrap();
    }

    store
        .ensure_activation_capacity_with_limit(3, &mut AssetLoadBudget::default())
        .unwrap();

    let remaining = fs::read_dir(&activations)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(
        remaining,
        [std::ffi::OsString::from(activation_file_name(
            active_ordinal
        ))]
    );
}

#[test]
fn repeated_activation_cleanup_failure_rejects_the_multiply_linked_head() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    let mut store = open_store(temporary.path(), options).unwrap();
    let mut build = store.begin().unwrap();
    write_artifacts(&build, "cleanup-retry-failure");
    let manifest = manifest_for(&store, &build, "cleanup-retry-failure", None);
    let report = store
        .prepare_publish_with_failpoint(
            &mut build,
            manifest,
            GenerationFailpoint::ActivationCleanup,
        )
        .unwrap()
        .activate()
        .unwrap();
    let recovery = activation_recovery_file_name(report.active.activation_ordinal());
    let pending = activation_pending_file_name(report.active.activation_ordinal());

    drop(build);
    drop(store);
    let error = open_store_with_startup_failpoint(
        temporary.path(),
        options,
        GenerationFailpoint::StartupStagingCleanup,
    )
    .unwrap_err();
    let GenerationStoreError::PersistedIdentityChanged { path } = error else {
        panic!("unexpected multiply-linked activation error: {error:?}");
    };
    assert!(
        matches!(path.file_name(), Some(name) if name == std::ffi::OsStr::new(&recovery) || name == std::ffi::OsStr::new(&pending))
    );
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
fn newest_obsolete_activation_never_falls_back_to_an_older_current_generation() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    let mut store = open_store(temporary.path(), options).unwrap();
    let current = publish_generation(&mut store, "current", None);
    let current_directory = store.generation_directory(current);
    drop(store);
    let (obsolete, _) = write_activation_record(
        temporary.path(),
        2,
        super::GENERATION_HEAD_CONTRACT_VERSION,
        Some(1),
    );

    let opened = open_store_with_startup_disposition(temporary.path(), options).unwrap();
    let (store, recovery, disposition) = opened.into_parts();
    assert_eq!(store.active(), None);
    assert_eq!(recovery.unwrap().removed_entries(), 1);
    assert_rebuild_required(
        disposition,
        IndexRebuildReason::ObsoleteGenerationStorage { actual: 1 },
        2,
        obsolete,
    );
    assert!(!current_directory.exists());
}

#[test]
fn obsolete_activation_bootstrap_survives_reopen_until_current_activation_commits() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    drop(open_store(temporary.path(), options).unwrap());
    let (obsolete, obsolete_activation) = write_activation_record(
        temporary.path(),
        1,
        super::REVISIONED_ACTIVATION_CONTRACT_VERSION,
        None,
    );

    let opened = open_store_with_startup_disposition(temporary.path(), options).unwrap();
    let (store, recovery, disposition) = opened.into_parts();
    assert_eq!(store.active(), None);
    assert_eq!(recovery.unwrap().removed_entries(), 0);
    assert_rebuild_required(
        disposition,
        IndexRebuildReason::ObsoleteActivationContract { actual: 2 },
        1,
        obsolete,
    );
    assert!(obsolete_activation.is_file());
    drop(store);

    let reopened = open_store_with_startup_disposition(temporary.path(), options).unwrap();
    let (mut store, recovery, disposition) = reopened.into_parts();
    assert_eq!(recovery.unwrap().removed_entries(), 0);
    assert_rebuild_required(
        disposition,
        IndexRebuildReason::ObsoleteActivationContract { actual: 2 },
        1,
        obsolete,
    );
    assert!(obsolete_activation.is_file());

    let rebuilt = publish_generation(&mut store, "rebuilt", None);

    assert!(!obsolete_activation.exists());
    assert_eq!(
        fs::read_dir(temporary.path().join(super::ACTIVATIONS_DIRECTORY))
            .unwrap()
            .count(),
        1
    );
    drop(store);

    let reopened = open_store(temporary.path(), options).unwrap();
    assert_eq!(reopened.active().unwrap().generation(), rebuilt);
}

#[test]
fn activation_sync_failure_recovers_the_new_head_if_its_final_link_is_lost() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    drop(open_store(temporary.path(), options).unwrap());
    let (obsolete, obsolete_activation) = write_activation_record(
        temporary.path(),
        1,
        super::REVISIONED_ACTIVATION_CONTRACT_VERSION,
        None,
    );

    let activations = temporary.path().join(super::ACTIVATIONS_DIRECTORY);
    let opened = open_store_with_startup_disposition(temporary.path(), options).unwrap();
    let (mut store, recovery, disposition) = opened.into_parts();
    assert_eq!(recovery.unwrap().removed_entries(), 0);
    assert_rebuild_required(
        disposition,
        IndexRebuildReason::ObsoleteActivationContract { actual: 2 },
        1,
        obsolete,
    );

    let mut build = store.begin().unwrap();
    write_artifacts(&build, "rebuilt-unsynced");
    let manifest = manifest_for(&store, &build, "rebuilt-unsynced", None);
    let report = store
        .prepare_publish_with_failpoint(
            &mut build,
            manifest,
            GenerationFailpoint::ActivationDirectorySync,
        )
        .unwrap()
        .activate()
        .unwrap();
    let rebuilt = report.active.generation();
    let new_activation = activations.join(activation_file_name(report.active.activation_ordinal()));
    assert!(report.warnings.iter().any(|warning| {
        warning.kind() == GenerationPublishWarningKind::PostCommitDurability
            && warning.message().contains("ActivationDirectorySync")
    }));
    assert!(obsolete_activation.is_file());
    assert!(new_activation.is_file());

    drop(build);
    drop(store);
    // Simulate a system crash losing the directory entry whose sync was not confirmed.
    fs::remove_file(&new_activation).unwrap();

    let reopened = open_store_with_startup_disposition(temporary.path(), options).unwrap();
    let (store, recovery, disposition) = reopened.into_parts();
    assert_eq!(recovery.unwrap().removed_entries(), 2);
    assert_eq!(disposition, GenerationStartupDisposition::Ready);
    assert_eq!(store.active().unwrap().generation(), rebuilt);
    assert!(obsolete_activation.is_file());
}

#[test]
fn surviving_newer_current_head_supersedes_an_obsolete_bootstrap() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions::default();
    drop(open_store(temporary.path(), options).unwrap());
    let (_, obsolete_activation) = write_activation_record(
        temporary.path(),
        1,
        super::REVISIONED_ACTIVATION_CONTRACT_VERSION,
        None,
    );

    let opened = open_store_with_startup_disposition(temporary.path(), options).unwrap();
    let (mut store, recovery, disposition) = opened.into_parts();
    assert_eq!(recovery.unwrap().removed_entries(), 0);
    assert!(matches!(
        disposition,
        GenerationStartupDisposition::RebuildRequired(_)
    ));

    let mut build = store.begin().unwrap();
    write_artifacts(&build, "rebuilt-unsynced");
    let manifest = manifest_for(&store, &build, "rebuilt-unsynced", None);
    let report = store
        .prepare_publish_with_failpoint(
            &mut build,
            manifest,
            GenerationFailpoint::ActivationDirectorySync,
        )
        .unwrap()
        .activate()
        .unwrap();
    let rebuilt = report.active.generation();
    assert!(obsolete_activation.is_file());

    drop(build);
    drop(store);
    let reopened = open_store_with_startup_disposition(temporary.path(), options).unwrap();
    let (store, recovery, disposition) = reopened.into_parts();
    assert_eq!(recovery.unwrap().removed_entries(), 2);
    assert_eq!(disposition, GenerationStartupDisposition::Ready);
    assert_eq!(store.active().unwrap().generation(), rebuilt);
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
        .estimate_publish(
            SearchGenerationId::new(digest("incoming")),
            4_096,
            &mut AssetLoadBudget::default(),
        )
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

#[test]
fn disk_estimate_binds_the_active_size_to_its_physical_storage_directory() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let generation = publish_generation(&mut store, "physical-active", None);
    let active_directory = store.active().unwrap().directory().to_path_buf();
    let active_bytes =
        super::tree_size_no_follow(&active_directory, &mut AssetLoadBudget::default()).unwrap();

    let obsolete_directory =
        active_directory.with_file_name(generation.directory_name_for_storage_contract(
            LEGACY_COUPLED_GENERATION_STORAGE_CONTRACT_VERSION,
        ));
    fs::create_dir(&obsolete_directory).unwrap();
    fs::write(
        obsolete_directory.join("retention-residue"),
        vec![0_u8; 64 * 1024],
    )
    .unwrap();

    let estimate = store
        .estimate_publish(
            SearchGenerationId::new(digest("incoming")),
            4_096,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(estimate.old_active_generation_bytes, active_bytes);
    assert!(estimate.existing_generation_bytes > active_bytes);
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
