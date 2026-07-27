use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use unity_asset::workspace::{
    AssetWorkspace, SourceAdmissionBatch, SourceAdmissionBatchPhase, SourceAdmissionDisposition,
    SourceAdmissionErrorCategory, SourceAdmissionFailure, SourceAdmissionFailureSite,
    SourceAdmissionOperation, SourceAdmissionOperationLocation, SourceAdmissionPolicy,
    SourceOpenRequest, WorkspaceView,
};
use unity_asset::{AssetLoadBudget, AssetLoadLimits, SourceAlias, SourceId, SourceKind};

fn source_path(directory: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, bytes).expect("write source fixture");
    path
}

fn raw_load(path: PathBuf, alias: &str, bytes: &[u8]) -> SourceAdmissionOperation {
    SourceAdmissionOperation::LoadBytes {
        request: SourceOpenRequest::new(path, SourceAlias::new(alias).expect("valid alias"))
            .with_kind_hint(SourceKind::StreamedResource),
        image: Arc::from(bytes),
    }
}

fn invalid_binary_load(path: PathBuf, alias: &str) -> SourceAdmissionOperation {
    SourceAdmissionOperation::LoadBytes {
        request: SourceOpenRequest::new(path, SourceAlias::new(alias).expect("valid alias"))
            .with_kind_hint(SourceKind::SerializedFile),
        image: Arc::from(b"not a serialized file".as_slice()),
    }
}

fn source_count(workspace: &AssetWorkspace) -> usize {
    workspace
        .snapshot()
        .sources(&mut AssetLoadBudget::default())
        .expect("list workspace sources")
        .len()
}

fn load_root(workspace: &mut AssetWorkspace, path: &Path, alias: &str, bytes: &[u8]) -> SourceId {
    workspace
        .load_source_bytes(
            SourceOpenRequest::new(path, SourceAlias::new(alias).expect("valid alias"))
                .with_kind_hint(SourceKind::StreamedResource),
            Arc::from(bytes),
            &mut AssetLoadBudget::default(),
        )
        .expect("load root source")
}

fn source_batch(
    operations: Vec<SourceAdmissionOperation>,
    budget: &mut AssetLoadBudget,
) -> SourceAdmissionBatch {
    let mut batch = SourceAdmissionBatch::with_capacity(operations.len(), budget)
        .expect("reserve source admission batch");
    for operation in operations {
        batch
            .try_push(operation, budget)
            .expect("fill reserved batch");
    }
    batch
}

#[test]
fn strict_middle_prepare_failure_rolls_back_complete_batch() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let first = source_path(directory.path(), "first.resource", b"first");
    let invalid = source_path(directory.path(), "invalid.assets", b"invalid");
    let last = source_path(directory.path(), "last.resource", b"last");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let base_revision = workspace.revision();
    let mut budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![
            raw_load(first, "first.resource", b"first"),
            invalid_binary_load(invalid, "invalid.assets"),
            raw_load(last, "last.resource", b"last"),
        ],
        &mut budget,
    );

    let error = workspace
        .admit_sources(batch, SourceAdmissionPolicy::Strict, &mut budget)
        .expect_err("strict content failure must reject the batch");

    assert_eq!(error.operation_ordinal(), Some(1));
    assert!(matches!(
        error.site(),
        SourceAdmissionFailureSite::Operation {
            ordinal: 1,
            location: Some(SourceAdmissionOperationLocation::PhysicalOrigin(path)),
        } if path.ends_with("invalid.assets")
    ));
    assert_eq!(error.category(), SourceAdmissionErrorCategory::Content);
    assert_eq!(workspace.revision(), base_revision);
    assert_eq!(source_count(&workspace), 0);
}

#[test]
fn strict_last_prepare_failure_rolls_back_complete_batch() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let first = source_path(directory.path(), "first.resource", b"first");
    let invalid = source_path(directory.path(), "invalid.assets", b"invalid");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let base_revision = workspace.revision();
    let mut budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![
            raw_load(first, "first.resource", b"first"),
            invalid_binary_load(invalid, "invalid.assets"),
        ],
        &mut budget,
    );

    let error = workspace
        .admit_sources(batch, SourceAdmissionPolicy::Strict, &mut budget)
        .expect_err("strict last failure must reject the batch");

    assert_eq!(error.operation_ordinal(), Some(1));
    assert_eq!(workspace.revision(), base_revision);
    assert_eq!(source_count(&workspace), 0);
}

#[test]
fn tolerant_content_rejection_and_success_install_one_revision() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let first = source_path(directory.path(), "first.resource", b"first");
    let invalid = source_path(directory.path(), "invalid.assets", b"invalid");
    let second = source_path(directory.path(), "second.resource", b"second");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let base_revision = workspace.revision();
    let mut budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![
            raw_load(first, "first.resource", b"first"),
            invalid_binary_load(invalid, "invalid.assets"),
            raw_load(second, "second.resource", b"second"),
        ],
        &mut budget,
    );

    let report = workspace
        .admit_sources(batch, SourceAdmissionPolicy::TolerantContent, &mut budget)
        .expect("content rejection is reportable");

    assert!(report.state_installed());
    assert_eq!(report.base_revision(), base_revision);
    assert_eq!(report.revision(), workspace.revision());
    assert_ne!(report.revision(), base_revision);
    assert_eq!(report.outcomes().len(), 3);
    assert_eq!(report.outcomes()[0].operation_ordinal(), 0);
    assert!(matches!(
        report.outcomes()[0].disposition(),
        SourceAdmissionDisposition::Loaded { .. }
    ));
    assert_eq!(report.outcomes()[1].operation_ordinal(), 1);
    assert_eq!(
        report.outcomes()[1]
            .disposition()
            .rejection()
            .expect("rejected content")
            .category(),
        SourceAdmissionErrorCategory::Content
    );
    assert_eq!(report.outcomes()[2].operation_ordinal(), 2);
    assert!(matches!(
        report.outcomes()[2].disposition(),
        SourceAdmissionDisposition::Loaded { .. }
    ));
    assert_eq!(source_count(&workspace), 2);
}

#[test]
fn tolerant_rejected_load_does_not_reserve_its_alias() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let invalid = source_path(directory.path(), "invalid.assets", b"invalid");
    let valid = source_path(directory.path(), "valid.resource", b"valid");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let mut budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![
            invalid_binary_load(invalid, "shared.resource"),
            raw_load(valid, "shared.resource", b"valid"),
        ],
        &mut budget,
    );

    let report = workspace
        .admit_sources(batch, SourceAdmissionPolicy::TolerantContent, &mut budget)
        .expect("rejected load must release its alias");

    assert!(report.state_installed());
    assert_eq!(report.outcomes().len(), 2);
    assert_eq!(report.outcomes()[0].operation_ordinal(), 0);
    assert_eq!(
        report.outcomes()[0]
            .disposition()
            .rejection()
            .expect("invalid content rejection")
            .category(),
        SourceAdmissionErrorCategory::Content
    );
    assert_eq!(report.outcomes()[1].operation_ordinal(), 1);
    assert!(matches!(
        report.outcomes()[1].disposition(),
        SourceAdmissionDisposition::Loaded { .. }
    ));
    assert_eq!(source_count(&workspace), 1);
}

#[test]
fn tolerant_rejected_load_does_not_reserve_its_physical_origin() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let shared = source_path(directory.path(), "shared.resource", b"valid");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let mut budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![
            invalid_binary_load(shared.clone(), "invalid.assets"),
            raw_load(shared, "valid.resource", b"valid"),
        ],
        &mut budget,
    );

    let report = workspace
        .admit_sources(batch, SourceAdmissionPolicy::TolerantContent, &mut budget)
        .expect("rejected load must release its physical origin");

    assert!(report.state_installed());
    assert_eq!(report.outcomes().len(), 2);
    assert_eq!(report.outcomes()[0].operation_ordinal(), 0);
    assert_eq!(
        report.outcomes()[0]
            .disposition()
            .rejection()
            .expect("invalid content rejection")
            .category(),
        SourceAdmissionErrorCategory::Content
    );
    assert_eq!(report.outcomes()[1].operation_ordinal(), 1);
    assert!(matches!(
        report.outcomes()[1].disposition(),
        SourceAdmissionDisposition::Loaded { .. }
    ));
    assert_eq!(source_count(&workspace), 1);
}

#[test]
fn all_rejected_and_same_content_noop_do_not_install_state() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let invalid_first = source_path(directory.path(), "first.assets", b"invalid");
    let invalid_second = source_path(directory.path(), "second.assets", b"invalid");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let empty_revision = workspace.revision();
    let mut rejected_budget = AssetLoadBudget::default();
    let rejected = source_batch(
        vec![
            invalid_binary_load(invalid_first, "first.assets"),
            invalid_binary_load(invalid_second, "second.assets"),
        ],
        &mut rejected_budget,
    );

    let rejected_report = workspace
        .admit_sources(
            rejected,
            SourceAdmissionPolicy::TolerantContent,
            &mut rejected_budget,
        )
        .expect("all content failures are reportable");

    assert!(!rejected_report.state_installed());
    assert_eq!(rejected_report.revision(), empty_revision);
    assert!(
        rejected_report
            .outcomes()
            .iter()
            .all(|outcome| outcome.disposition().rejection().is_some())
    );

    let raw_path = source_path(directory.path(), "same.resource", b"same");
    let alias = SourceAlias::new("same.resource").expect("valid alias");
    let source = workspace
        .load_source_bytes(
            SourceOpenRequest::new(&raw_path, alias.clone())
                .with_kind_hint(SourceKind::StreamedResource),
            Arc::from(b"same".as_slice()),
            &mut AssetLoadBudget::default(),
        )
        .expect("load initial source");
    let loaded_revision = workspace.revision();
    let mut no_op_budget = AssetLoadBudget::default();
    let no_op = source_batch(
        vec![raw_load(raw_path, alias.as_str(), b"same")],
        &mut no_op_budget,
    );

    let no_op_report = workspace
        .admit_sources(no_op, SourceAdmissionPolicy::Strict, &mut no_op_budget)
        .expect("same content is a no-op");

    assert!(!no_op_report.state_installed());
    assert_eq!(no_op_report.revision(), loaded_revision);
    assert!(matches!(
        no_op_report.outcomes()[0].disposition(),
        SourceAdmissionDisposition::Unchanged { source_id } if *source_id == source
    ));
}

#[test]
fn tolerant_rejected_open_does_not_discard_ordered_unload() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let loaded_path = source_path(directory.path(), "loaded.resource", b"loaded");
    let invalid_path = source_path(directory.path(), "invalid.assets", b"invalid");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let source = workspace
        .load_source_bytes(
            SourceOpenRequest::new(
                &loaded_path,
                SourceAlias::new("loaded.resource").expect("valid alias"),
            )
            .with_kind_hint(SourceKind::StreamedResource),
            Arc::from(b"loaded".as_slice()),
            &mut AssetLoadBudget::default(),
        )
        .expect("load initial source");
    let loaded_revision = workspace.revision();
    let mut budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![
            SourceAdmissionOperation::Unload(source),
            invalid_binary_load(invalid_path, "invalid.assets"),
        ],
        &mut budget,
    );

    let report = workspace
        .admit_sources(batch, SourceAdmissionPolicy::TolerantContent, &mut budget)
        .expect("unload and rejection can commit together");

    assert!(report.state_installed());
    assert_ne!(report.revision(), loaded_revision);
    assert!(matches!(
        report.outcomes()[0].disposition(),
        SourceAdmissionDisposition::Unloaded { source_id } if *source_id == source
    ));
    assert!(matches!(
        report.outcomes()[1].disposition(),
        SourceAdmissionDisposition::Rejected(_)
    ));
    assert_eq!(source_count(&workspace), 0);
}

#[test]
fn tolerant_policy_never_downgrades_budget_failure() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let path = source_path(directory.path(), "limited.resource", b"limited");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let base_revision = workspace.revision();
    let mut measured_batch_budget = AssetLoadBudget::default();
    let measured_batch = source_batch(
        vec![raw_load(path.clone(), "limited.resource", b"limited")],
        &mut measured_batch_budget,
    );
    let batch_bytes = measured_batch_budget.usage().bytes;
    drop(measured_batch);
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: batch_bytes + 1,
        ..Default::default()
    })
    .expect("valid budget limits");
    let batch = source_batch(
        vec![raw_load(path.clone(), "limited.resource", b"limited")],
        &mut budget,
    );

    let error = workspace
        .admit_sources(batch, SourceAdmissionPolicy::TolerantContent, &mut budget)
        .expect_err("budget failures are fatal");

    assert_eq!(error.category(), SourceAdmissionErrorCategory::Budget);
    assert_eq!(error.operation_ordinal(), None);
    assert_eq!(
        error.batch_phase(),
        Some(SourceAdmissionBatchPhase::Preparation)
    );
    assert_eq!(workspace.revision(), base_revision);
    assert_eq!(source_count(&workspace), 0);
}

#[test]
fn late_apply_failure_discards_earlier_candidate_changes() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let path = source_path(directory.path(), "candidate.resource", b"candidate");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let base_revision = workspace.revision();
    let missing = SourceId::new(workspace.workspace_id(), SourceKind::StreamedResource, 77)
        .expect("valid unknown source identity");
    let mut budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![
            raw_load(path, "candidate.resource", b"candidate"),
            SourceAdmissionOperation::Unload(missing),
        ],
        &mut budget,
    );

    let error = workspace
        .admit_sources(batch, SourceAdmissionPolicy::TolerantContent, &mut budget)
        .expect_err("late apply failure rejects the candidate");

    assert_eq!(error.operation_ordinal(), Some(1));
    assert!(matches!(
        error.operation_location(),
        Some(SourceAdmissionOperationLocation::Source(source)) if *source == missing
    ));
    assert_eq!(
        error.category(),
        SourceAdmissionErrorCategory::WorkspaceInvariant
    );
    assert_eq!(workspace.revision(), base_revision);
    assert_eq!(source_count(&workspace), 0);
}

#[test]
fn duplicate_alias_and_physical_origin_are_typed_ordered_rejections() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let first_path = source_path(directory.path(), "first.resource", b"first");
    let second_path = source_path(directory.path(), "second.resource", b"second");
    let mut alias_workspace = AssetWorkspace::new().expect("create workspace");
    let mut alias_budget = AssetLoadBudget::default();
    let alias_batch = source_batch(
        vec![
            raw_load(first_path, "same.resource", b"first"),
            raw_load(second_path, "same.resource", b"second"),
        ],
        &mut alias_budget,
    );

    let alias_report = alias_workspace
        .admit_sources(
            alias_batch,
            SourceAdmissionPolicy::TolerantContent,
            &mut alias_budget,
        )
        .expect("duplicate alias is reportable");
    let alias_rejection = alias_report.outcomes()[1]
        .disposition()
        .rejection()
        .expect("duplicate alias rejection");
    assert_eq!(
        alias_rejection.category(),
        SourceAdmissionErrorCategory::DuplicateAlias
    );
    assert_eq!(alias_rejection.failure().first_operation(), Some(0));
    assert!(matches!(
        alias_rejection.operation_location(),
        Some(SourceAdmissionOperationLocation::Alias(alias)) if alias.as_str() == "same.resource"
    ));
    assert_eq!(source_count(&alias_workspace), 1);

    let shared_path = source_path(directory.path(), "shared.resource", b"shared");
    let mut origin_workspace = AssetWorkspace::new().expect("create workspace");
    let mut origin_budget = AssetLoadBudget::default();
    let origin_batch = source_batch(
        vec![
            raw_load(shared_path.clone(), "first.resource", b"shared"),
            raw_load(shared_path, "second.resource", b"shared"),
        ],
        &mut origin_budget,
    );

    let origin_report = origin_workspace
        .admit_sources(
            origin_batch,
            SourceAdmissionPolicy::TolerantContent,
            &mut origin_budget,
        )
        .expect("duplicate physical origin is reportable");
    let origin_rejection = origin_report.outcomes()[1]
        .disposition()
        .rejection()
        .expect("duplicate origin rejection");
    assert_eq!(
        origin_rejection.category(),
        SourceAdmissionErrorCategory::DuplicatePhysicalOrigin
    );
    assert_eq!(origin_rejection.failure().first_operation(), Some(0));
    assert!(matches!(
        origin_rejection.operation_location(),
        Some(SourceAdmissionOperationLocation::PhysicalOrigin(path))
            if path.ends_with("shared.resource")
    ));
    assert_eq!(source_count(&origin_workspace), 1);
}

#[test]
fn tolerant_existing_root_identity_conflicts_are_ordered_rejections() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let existing_path = source_path(directory.path(), "existing.resource", b"existing");
    let alias_conflict_path = source_path(directory.path(), "alias-conflict.resource", b"changed");
    let accepted_path = source_path(directory.path(), "accepted.resource", b"accepted");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let existing = workspace
        .load_source_bytes(
            SourceOpenRequest::new(
                &existing_path,
                SourceAlias::new("existing.resource").expect("valid alias"),
            )
            .with_kind_hint(SourceKind::StreamedResource),
            Arc::from(b"existing".as_slice()),
            &mut AssetLoadBudget::default(),
        )
        .expect("load existing root");
    let base_revision = workspace.revision();
    let mut budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![
            raw_load(alias_conflict_path, "existing.resource", b"changed"),
            raw_load(existing_path, "renamed.resource", b"existing"),
            raw_load(accepted_path, "accepted.resource", b"accepted"),
        ],
        &mut budget,
    );

    let report = workspace
        .admit_sources(batch, SourceAdmissionPolicy::TolerantContent, &mut budget)
        .expect("existing identity conflicts are reportable");

    assert!(report.state_installed());
    assert_ne!(report.revision(), base_revision);
    assert_eq!(report.outcomes().len(), 3);
    let alias_conflict = report.outcomes()[0]
        .disposition()
        .rejection()
        .expect("alias conflict is rejected");
    assert_eq!(
        alias_conflict.category(),
        SourceAdmissionErrorCategory::Identity
    );
    assert!(matches!(
        alias_conflict.failure(),
        SourceAdmissionFailure::AliasConflict { existing_source } if *existing_source == existing
    ));
    assert!(matches!(
        alias_conflict.operation_location(),
        Some(SourceAdmissionOperationLocation::Alias(alias))
            if alias.as_str() == "existing.resource"
    ));
    let physical_conflict = report.outcomes()[1]
        .disposition()
        .rejection()
        .expect("physical origin conflict is rejected");
    assert_eq!(
        physical_conflict.category(),
        SourceAdmissionErrorCategory::Identity
    );
    assert!(matches!(
        physical_conflict.failure(),
        SourceAdmissionFailure::PhysicalOriginConflict { existing_source }
            if *existing_source == existing
    ));
    assert!(matches!(
        physical_conflict.operation_location(),
        Some(SourceAdmissionOperationLocation::PhysicalOrigin(path))
            if path.ends_with("existing.resource")
    ));
    assert!(matches!(
        report.outcomes()[2].disposition(),
        SourceAdmissionDisposition::Loaded { .. }
    ));
    assert_eq!(source_count(&workspace), 2);
}

#[test]
fn rejected_load_then_unload_then_identical_load_observes_candidate_order() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let path = source_path(directory.path(), "existing.resource", b"existing");
    let alias = "existing.resource";
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let existing = load_root(&mut workspace, &path, alias, b"existing");
    let mut budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![
            raw_load(path.clone(), alias, b"replacement"),
            SourceAdmissionOperation::Unload(existing),
            raw_load(path, alias, b"replacement"),
        ],
        &mut budget,
    );

    let report = workspace
        .admit_sources(batch, SourceAdmissionPolicy::TolerantContent, &mut budget)
        .expect("rejected load must not reserve a later identity");

    assert_eq!(report.outcomes().len(), 3);
    assert_eq!(report.outcomes()[0].operation_ordinal(), 0);
    assert_eq!(report.outcomes()[1].operation_ordinal(), 1);
    assert_eq!(report.outcomes()[2].operation_ordinal(), 2);
    assert!(report.state_installed());
    assert!(matches!(
        report.outcomes()[0].disposition().rejection(),
        Some(rejection) if rejection.category() == SourceAdmissionErrorCategory::Identity
    ));
    assert!(matches!(
        report.outcomes()[1].disposition(),
        SourceAdmissionDisposition::Unloaded { source_id } if *source_id == existing
    ));
    assert!(matches!(
        report.outcomes()[2].disposition(),
        SourceAdmissionDisposition::Loaded { .. }
    ));
    assert_eq!(source_count(&workspace), 1);
}

#[test]
fn rejected_load_does_not_poison_the_physical_origin_index() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let existing_path = source_path(directory.path(), "existing.resource", b"existing");
    let retry_path = source_path(directory.path(), "retry.resource", b"replacement");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let existing = load_root(
        &mut workspace,
        &existing_path,
        "existing.resource",
        b"existing",
    );
    let mut budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![
            raw_load(retry_path.clone(), "existing.resource", b"replacement"),
            SourceAdmissionOperation::Unload(existing),
            raw_load(retry_path, "replacement.resource", b"replacement"),
        ],
        &mut budget,
    );

    let report = workspace
        .admit_sources(batch, SourceAdmissionPolicy::TolerantContent, &mut budget)
        .expect("rejected alias conflict must not reserve its physical origin");

    assert!(matches!(
        report.outcomes()[0].disposition().rejection(),
        Some(rejection) if matches!(rejection.failure(), SourceAdmissionFailure::AliasConflict { .. })
    ));
    assert!(matches!(
        report.outcomes()[2].disposition(),
        SourceAdmissionDisposition::Loaded { .. }
    ));
    assert_eq!(source_count(&workspace), 1);
}

#[test]
fn rejected_load_does_not_poison_the_alias_index() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let existing_path = source_path(directory.path(), "existing.resource", b"existing");
    let retry_path = source_path(directory.path(), "retry.resource", b"replacement");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let existing = load_root(
        &mut workspace,
        &existing_path,
        "existing.resource",
        b"existing",
    );
    let mut budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![
            raw_load(existing_path, "replacement.resource", b"replacement"),
            SourceAdmissionOperation::Unload(existing),
            raw_load(retry_path, "replacement.resource", b"replacement"),
        ],
        &mut budget,
    );

    let report = workspace
        .admit_sources(batch, SourceAdmissionPolicy::TolerantContent, &mut budget)
        .expect("rejected physical conflict must not reserve its alias");

    assert!(matches!(
        report.outcomes()[0].disposition().rejection(),
        Some(rejection)
            if matches!(rejection.failure(), SourceAdmissionFailure::PhysicalOriginConflict { .. })
    ));
    assert!(matches!(
        report.outcomes()[2].disposition(),
        SourceAdmissionDisposition::Loaded { .. }
    ));
    assert_eq!(source_count(&workspace), 1);
}

#[test]
fn strict_existing_root_identity_conflict_discards_earlier_candidate_changes() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let existing_path = source_path(directory.path(), "existing.resource", b"existing");
    let candidate_path = source_path(directory.path(), "candidate.resource", b"candidate");
    let conflict_path = source_path(directory.path(), "conflict.resource", b"changed");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    workspace
        .load_source_bytes(
            SourceOpenRequest::new(
                &existing_path,
                SourceAlias::new("existing.resource").expect("valid alias"),
            )
            .with_kind_hint(SourceKind::StreamedResource),
            Arc::from(b"existing".as_slice()),
            &mut AssetLoadBudget::default(),
        )
        .expect("load existing root");
    let base_revision = workspace.revision();
    let mut budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![
            raw_load(candidate_path, "candidate.resource", b"candidate"),
            raw_load(conflict_path, "existing.resource", b"changed"),
        ],
        &mut budget,
    );

    let error = workspace
        .admit_sources(batch, SourceAdmissionPolicy::Strict, &mut budget)
        .expect_err("strict identity conflict must reject the batch");

    assert_eq!(error.operation_ordinal(), Some(1));
    assert_eq!(error.category(), SourceAdmissionErrorCategory::Identity);
    assert!(matches!(
        error.operation_location(),
        Some(SourceAdmissionOperationLocation::Alias(alias))
            if alias.as_str() == "existing.resource"
    ));
    assert_eq!(workspace.revision(), base_revision);
    assert_eq!(source_count(&workspace), 1);
}

#[test]
fn ordered_unload_then_reload_publishes_one_revision() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let path = source_path(directory.path(), "reload.resource", b"old");
    let alias = SourceAlias::new("reload.resource").expect("valid alias");
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let original = workspace
        .load_source_bytes(
            SourceOpenRequest::new(&path, alias.clone())
                .with_kind_hint(SourceKind::StreamedResource),
            Arc::from(b"old".as_slice()),
            &mut AssetLoadBudget::default(),
        )
        .expect("load original source");
    let base_revision = workspace.revision();
    let mut implicit_reload_budget = AssetLoadBudget::default();
    let implicit_reload = source_batch(
        vec![raw_load(path.clone(), alias.as_str(), b"new")],
        &mut implicit_reload_budget,
    );
    let implicit_report = workspace
        .admit_sources(
            implicit_reload,
            SourceAdmissionPolicy::TolerantContent,
            &mut implicit_reload_budget,
        )
        .expect("bound root fingerprint conflict is reportable");
    assert!(!implicit_report.state_installed());
    assert_eq!(implicit_report.revision(), base_revision);
    assert!(matches!(
        implicit_report.outcomes()[0].disposition().rejection(),
        Some(rejection)
            if rejection.category() == SourceAdmissionErrorCategory::Identity
                && matches!(
                    rejection.failure(),
                    SourceAdmissionFailure::AliasConflict { existing_source }
                        if *existing_source == original
                )
    ));
    let mut budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![
            SourceAdmissionOperation::Unload(original),
            raw_load(path, alias.as_str(), b"new"),
        ],
        &mut budget,
    );

    let report = workspace
        .admit_sources(batch, SourceAdmissionPolicy::Strict, &mut budget)
        .expect("ordered unload and reload");

    assert!(report.state_installed());
    assert_eq!(report.base_revision(), base_revision);
    assert_ne!(report.revision(), base_revision);
    assert_eq!(workspace.revision(), report.revision());
    assert!(matches!(
        report.outcomes()[0].disposition(),
        SourceAdmissionDisposition::Unloaded { source_id } if *source_id == original
    ));
    assert!(matches!(
        report.outcomes()[1].disposition(),
        SourceAdmissionDisposition::Loaded { source_id } if *source_id == original
    ));
    assert_eq!(source_count(&workspace), 1);
}

#[test]
fn one_short_publication_budget_reports_a_batch_site() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let path = source_path(directory.path(), "publication.resource", b"publication");
    let operation = || raw_load(path.clone(), "publication.resource", b"publication");

    let mut measured_workspace = AssetWorkspace::new().expect("create measured workspace");
    let mut measured_budget = AssetLoadBudget::default();
    let measured_batch = source_batch(vec![operation()], &mut measured_budget);
    measured_workspace
        .admit_sources(
            measured_batch,
            SourceAdmissionPolicy::Strict,
            &mut measured_budget,
        )
        .expect("measure successful publication");
    let measured_usage = measured_budget.usage();

    let mut exact_workspace = AssetWorkspace::new().expect("create exact workspace");
    let mut exact_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: measured_usage.bytes,
        ..AssetLoadLimits::default()
    })
    .expect("valid exact budget");
    let exact_batch = source_batch(vec![operation()], &mut exact_budget);
    exact_workspace
        .admit_sources(
            exact_batch,
            SourceAdmissionPolicy::Strict,
            &mut exact_budget,
        )
        .expect("exact publication budget");
    assert_eq!(exact_budget.usage().bytes, measured_usage.bytes);

    let mut rejected_workspace = AssetWorkspace::new().expect("create rejected workspace");
    let base_revision = rejected_workspace.revision();
    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: measured_usage.bytes - 1,
        ..AssetLoadLimits::default()
    })
    .expect("valid one-short budget");
    let rejected_batch = source_batch(vec![operation()], &mut one_short);
    let error = rejected_workspace
        .admit_sources(
            rejected_batch,
            SourceAdmissionPolicy::Strict,
            &mut one_short,
        )
        .expect_err("one-short publication must fail");

    assert_eq!(error.operation_ordinal(), None);
    assert_eq!(
        error.batch_phase(),
        Some(SourceAdmissionBatchPhase::Publication)
    );
    assert!(matches!(
        error.site(),
        SourceAdmissionFailureSite::Batch {
            phase: SourceAdmissionBatchPhase::Publication
        }
    ));
    assert_eq!(error.category(), SourceAdmissionErrorCategory::Budget);
    assert_eq!(rejected_workspace.revision(), base_revision);
    assert_eq!(source_count(&rejected_workspace), 0);
}

#[test]
fn foreign_batch_budget_reports_preparation_without_an_operation_path() {
    let mut workspace = AssetWorkspace::new().expect("create workspace");
    let base_revision = workspace.revision();
    let missing = SourceId::new(workspace.workspace_id(), SourceKind::StreamedResource, 99)
        .expect("valid source identity");
    let mut construction_budget = AssetLoadBudget::default();
    let batch = source_batch(
        vec![SourceAdmissionOperation::Unload(missing)],
        &mut construction_budget,
    );
    let mut foreign_budget = AssetLoadBudget::default();

    let error = workspace
        .admit_sources(batch, SourceAdmissionPolicy::Strict, &mut foreign_budget)
        .expect_err("foreign budget domain must be rejected");

    assert_eq!(error.operation_ordinal(), None);
    assert_eq!(
        error.batch_phase(),
        Some(SourceAdmissionBatchPhase::Preparation)
    );
    assert_eq!(error.operation_location(), None);
    assert_eq!(error.category(), SourceAdmissionErrorCategory::Budget);
    assert_eq!(workspace.revision(), base_revision);
    assert_eq!(foreign_budget.usage(), Default::default());
}
