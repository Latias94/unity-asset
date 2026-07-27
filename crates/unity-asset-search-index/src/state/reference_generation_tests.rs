use std::fs;
use std::path::Path;

use super::{
    GenerationBuild, GenerationFailpoint, GenerationPublishWarningKind, GenerationStore,
    GenerationStoreError, GenerationStoreOptions,
};
use crate::generation::{
    ArtifactTreeEvidence, GenerationArtifactEvidence, GenerationProjectionDigests, ReindexReceipt,
    SearchGenerationId, SearchGenerationIdentityV1, SearchGenerationManifestV1,
};
use serde::Serialize;
use tempfile::TempDir;
use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, AssetLoadUsage, BudgetError, BudgetedJsonError, DigestV1,
    WorkspaceId, WorkspaceRevision,
};

fn digest(label: &str) -> DigestV1 {
    DigestV1::hash_bytes(label.as_bytes())
}

fn revision(label: &str) -> WorkspaceRevision {
    WorkspaceRevision::new(digest(label))
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

fn source_state_payload(label: &str) -> (Vec<u8>, DigestV1) {
    #[derive(Serialize)]
    struct LogicalState<'state> {
        contract_version: u16,
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        transaction_receipts: &'state [()],
        assets: &'state [()],
    }

    #[derive(Serialize)]
    struct PersistedState<'state> {
        contract_version: u16,
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        transaction_receipts: &'state [()],
        scan_hints: &'state [()],
        assets: &'state [()],
        logical_digest: DigestV1,
    }

    let workspace = WorkspaceId::from_u128(0x9001).unwrap();
    let revision = revision(label);
    let assets = [];
    let logical = LogicalState {
        contract_version: 1,
        workspace,
        revision,
        transaction_receipts: &[],
        assets: &assets,
    };
    let logical_digest = DigestV1::hash_bytes(&serde_json::to_vec(&logical).unwrap());
    let persisted = PersistedState {
        contract_version: 1,
        workspace,
        revision,
        transaction_receipts: &[],
        scan_hints: &[],
        assets: &assets,
        logical_digest,
    };
    (serde_json::to_vec(&persisted).unwrap(), logical_digest)
}

fn write_artifacts(build: &GenerationBuild, label: &str) {
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
        build.source_state_directory().join("source-state-v1.json"),
        source_state_payload(label).0,
    )
    .unwrap();
}

fn manifest_for(
    store: &GenerationStore,
    build: &GenerationBuild,
    label: &str,
    parent: Option<SearchGenerationId>,
) -> SearchGenerationManifestV1 {
    let evidence = store.measure_artifacts(build).unwrap();
    let identity = SearchGenerationIdentityV1::new(
        WorkspaceId::from_u128(0x9001).unwrap(),
        revision(label),
        GenerationProjectionDigests::new(
            digest(&format!("search-projection:{label}")),
            digest(&format!("reference-projection:{label}")),
        ),
        Default::default(),
        parent,
        Vec::new(),
        digest("options"),
        source_state_payload(label).1,
    )
    .unwrap();
    SearchGenerationManifestV1::new(identity, evidence)
}

fn publish_generation(
    store: &mut GenerationStore,
    label: &str,
    parent: Option<SearchGenerationId>,
) -> SearchGenerationId {
    let build = store.begin().unwrap();
    write_artifacts(&build, label);
    let manifest = manifest_for(store, &build, label, parent);
    let prepared = store.prepare_publish(build, manifest).unwrap();
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
        Some(SearchGenerationId::new(digest("parent"))),
        digest("options"),
        digest("source state"),
    );
    let first_identity = SearchGenerationIdentityV1::new(
        arguments.0,
        arguments.1,
        GenerationProjectionDigests::new(arguments.2, arguments.3),
        Default::default(),
        arguments.4,
        Vec::new(),
        arguments.5,
        arguments.6,
    )
    .unwrap();
    let second_identity = SearchGenerationIdentityV1::new(
        arguments.0,
        arguments.1,
        GenerationProjectionDigests::new(arguments.2, arguments.3),
        Default::default(),
        arguments.4,
        Vec::new(),
        arguments.5,
        arguments.6,
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
    let encoded = directory_name.strip_prefix("generation-v1-").unwrap();
    let uppercase_alias = format!("generation-v1-{}", encoded.to_ascii_uppercase());
    assert_eq!(
        SearchGenerationId::from_directory_name(&uppercase_alias),
        None
    );
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
        None,
        Vec::new(),
        digest("options"),
        digest("state"),
    )
    .unwrap();
    let manifest = SearchGenerationManifestV1::new(identity, evidence);

    let mut unknown = serde_json::to_value(&manifest).unwrap();
    unknown
        .as_object_mut()
        .unwrap()
        .insert("unexpected".to_owned(), serde_json::json!(true));
    assert!(serde_json::from_value::<SearchGenerationManifestV1>(unknown).is_err());

    let mut unsupported = serde_json::to_value(&manifest).unwrap();
    unsupported["contract_version"] = serde_json::json!(2);
    assert!(serde_json::from_value::<SearchGenerationManifestV1>(unsupported).is_err());

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
fn reindex_receipt_defaults_missing_execution_evidence() {
    let receipt = serde_json::from_value::<ReindexReceipt>(serde_json::json!({
        "contract_version": 1,
        "disposition": "queued"
    }))
    .unwrap();

    assert!(!receipt.evidence.forced_full_scan);
    assert!(!receipt.evidence.forced_full_analysis);
    assert_eq!(receipt.evidence.dependency_closure_assets, 0);
    assert!(receipt.evidence.disk_estimate.is_none());
    assert!(receipt.evidence.publish_warnings.is_empty());
}

#[test]
fn reopen_ignores_incomplete_staging_and_malformed_activation() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let generation = publish_generation(&mut store, "baseline", None);

    let incomplete = store.begin().unwrap();
    fs::write(incomplete.search_directory().join("partial"), b"partial").unwrap();
    fs::write(
        temporary
            .path()
            .join("activations")
            .join("00000000000000000999.json"),
        b"{",
    )
    .unwrap();
    drop(incomplete);
    drop(store);

    let mut reopened = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    assert_eq!(reopened.active().unwrap().generation(), generation);
    let next = publish_generation(&mut reopened, "after-gap", Some(generation));
    assert_eq!(reopened.active().unwrap().activation_ordinal(), 1_000);
    assert_eq!(reopened.active().unwrap().generation(), next);
}

#[test]
fn reopen_budget_is_exact_and_corrupt_candidate_work_accumulates() {
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
                .join(format!("{ordinal:020}.json")),
            b"{",
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
fn activation_contract_rejects_deep_wide_and_trailing_candidates() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);
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
    fs::write(
        activations.join("00000000000000000100.json"),
        serde_json::to_vec(&deep).unwrap(),
    )
    .unwrap();
    fs::write(
        activations.join("00000000000000000101.json"),
        serde_json::to_vec(&vec![0_u8; 64]).unwrap(),
    )
    .unwrap();
    let mut trailing = valid_activation;
    trailing.extend_from_slice(b" null");
    fs::write(activations.join("00000000000000000102.json"), trailing).unwrap();

    let reopened = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    assert_eq!(reopened.active().unwrap().generation(), baseline);
}

#[test]
fn manifest_contract_rejects_deep_wide_and_trailing_candidates() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 8,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    let first = publish_generation(&mut store, "first", None);
    let second = publish_generation(&mut store, "second", Some(first));
    let second_directory = store.generation_directory(second);
    let third = publish_generation(&mut store, "third", Some(second));
    let third_directory = store.generation_directory(third);
    let fourth = publish_generation(&mut store, "fourth", Some(third));
    let fourth_directory = store.generation_directory(fourth);
    drop(store);

    let mut deep = serde_json::json!(0);
    for _ in 0..10 {
        deep = serde_json::json!([deep]);
    }
    fs::write(
        second_directory.join("manifest.json"),
        serde_json::to_vec(&deep).unwrap(),
    )
    .unwrap();
    fs::write(
        third_directory.join("manifest.json"),
        serde_json::to_vec(&vec![0_u8; 5_000]).unwrap(),
    )
    .unwrap();
    let manifest_path = fourth_directory.join("manifest.json");
    let mut trailing = fs::read(&manifest_path).unwrap();
    trailing.extend_from_slice(b" null");
    fs::write(manifest_path, trailing).unwrap();

    let reopened = open_store(temporary.path(), options).unwrap();
    assert_eq!(reopened.active().unwrap().generation(), first);
}

#[test]
fn corrupted_latest_generation_falls_back_to_previous_activation() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 1,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    let first = publish_generation(&mut store, "first", None);
    let second = publish_generation(&mut store, "second", Some(first));
    let second_search = store.active().unwrap().search_directory().join("segments");
    fs::write(second_search, b"corrupt").unwrap();
    drop(store);

    let reopened = open_store(temporary.path(), options).unwrap();
    assert_eq!(reopened.active().unwrap().generation(), first);
    assert_ne!(reopened.active().unwrap().generation(), second);
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
        let build = store.begin().unwrap();
        write_artifacts(&build, &label);
        let manifest = manifest_for(&store, &build, &label, Some(baseline));
        let error = store
            .prepare_publish_with_failpoint(build, manifest, failpoint)
            .and_then(|prepared| prepared.activate())
            .unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::InjectedFailure { checkpoint }
                if checkpoint == failpoint
        ));
        assert_eq!(store.active().unwrap().generation(), baseline);

        drop(store);
        store = open_store(temporary.path(), options).unwrap();
        assert_eq!(store.active().unwrap().generation(), baseline);
    }

    let retry = store.begin().unwrap();
    write_artifacts(&retry, "candidate-3");
    let retry_manifest = manifest_for(&store, &retry, "candidate-3", Some(baseline));
    let repaired = store
        .prepare_publish(retry, retry_manifest)
        .unwrap()
        .activate()
        .unwrap();
    assert_ne!(repaired.active.generation(), baseline);

    drop(store);
    let reopened = open_store(temporary.path(), options).unwrap();
    assert_eq!(
        reopened.active().unwrap().generation(),
        repaired.active.generation()
    );
}

#[test]
fn activation_directory_sync_failure_returns_committed_generation_consistent_with_reopen() {
    let temporary = TempDir::new().unwrap();
    let options = GenerationStoreOptions {
        retain_previous_generations: 1,
    };
    let mut store = open_store(temporary.path(), options).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);
    let build = store.begin().unwrap();
    write_artifacts(&build, "sync-warning");
    let manifest = manifest_for(&store, &build, "sync-warning", Some(baseline));
    let candidate = manifest.generation_id();

    let report = store
        .prepare_publish_with_failpoint(
            build,
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
    let build = store.begin().unwrap();
    write_artifacts(&build, "cleanup-warning");
    let manifest = manifest_for(&store, &build, "cleanup-warning", Some(baseline));
    let candidate = manifest.generation_id();

    let report = store
        .prepare_publish_with_failpoint(build, manifest, GenerationFailpoint::ActivationCleanup)
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
    let build = store.begin().unwrap();
    write_artifacts(&build, "candidate");
    let manifest = manifest_for(&store, &build, "candidate", Some(baseline));
    let candidate = manifest.generation_id();

    let prepared = store.prepare_publish(build, manifest).unwrap();
    assert_eq!(
        fs::read(prepared.snapshot().search_directory().join("segments")).unwrap(),
        b"search:candidate"
    );
    drop(prepared);
    assert_eq!(store.active().unwrap().generation(), baseline);

    let retry = store.begin().unwrap();
    write_artifacts(&retry, "candidate");
    let retry_manifest = manifest_for(&store, &retry, "candidate", Some(baseline));
    let prepared = store.prepare_publish(retry, retry_manifest).unwrap();
    assert_eq!(prepared.snapshot().generation(), candidate);
    let report = prepared.activate().unwrap();
    assert_eq!(report.active.generation(), candidate);
}

#[test]
fn publish_rejects_source_state_that_does_not_match_the_manifest() {
    let temporary = TempDir::new().unwrap();
    let mut store = open_store(temporary.path(), GenerationStoreOptions::default()).unwrap();
    let baseline = publish_generation(&mut store, "baseline", None);
    let build = store.begin().unwrap();
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
        Some(baseline),
        Vec::new(),
        digest("options"),
        digest("wrong-source-state"),
    )
    .unwrap();

    assert!(matches!(
        store.prepare_publish(build, SearchGenerationManifestV1::new(identity, evidence)),
        Err(GenerationStoreError::InvalidSourceState { .. })
    ));
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

    let build = store.begin().unwrap();
    write_artifacts(&build, "candidate");
    let manifest = manifest_for(&store, &build, "candidate", Some(baseline));
    let generation = manifest.generation_id();
    assert!(matches!(
        store
            .prepare_publish_with_failpoint(build, manifest, GenerationFailpoint::Activation)
            .and_then(|prepared| prepared.activate()),
        Err(GenerationStoreError::InjectedFailure {
            checkpoint: GenerationFailpoint::Activation
        })
    ));
    fs::write(
        store
            .generation_directory(generation)
            .join("search")
            .join("segments"),
        b"corrupt",
    )
    .unwrap();

    let retry = store.begin().unwrap();
    write_artifacts(&retry, "candidate");
    let retry_manifest = manifest_for(&store, &retry, "candidate", Some(baseline));
    let report = store
        .prepare_publish(retry, retry_manifest)
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

    let estimate = store.estimate_publish(4_096).unwrap();
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
    let build = store.begin().unwrap();
    write_artifacts(&build, "read-only");
    let artifact = build.search_directory().join("segments");
    let mut permissions = fs::metadata(&artifact).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&artifact, permissions).unwrap();
    let manifest = manifest_for(&store, &build, "read-only", None);

    let report = store
        .prepare_publish(build, manifest)
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
