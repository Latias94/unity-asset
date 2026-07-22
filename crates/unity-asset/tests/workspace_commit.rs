use std::fs;
use std::path::Path;

use tempfile::TempDir;
use unity_asset::workspace::{
    AssetWorkspace, CommitError, FieldGuard, GenericMutation, MutationPlan, MutationValue,
    PrepareOptions, PublicationTarget, SourceExpectation, SourceOpenRequest, WorkspaceLookup,
    WorkspaceView,
};
use unity_asset::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, FieldPath, ObjectAddress, SourceAlias,
    SourceFingerprint, SourceKind, SourceLocator, UnityValue,
};
use unity_asset_core::{semantic_value_digest, yaml_field_schema_digest};

const SOURCE_ALIAS: &str = "committed.prefab";
const YAML: &str =
    "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Before\n";

fn fixture() -> (TempDir, std::path::PathBuf, AssetWorkspace) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(SOURCE_ALIAS);
    fs::write(&path, YAML).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_source(
            SourceOpenRequest::new(&path, SourceAlias::new(SOURCE_ALIAS).unwrap())
                .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    (directory, path, workspace)
}

fn address() -> ObjectAddress {
    ObjectAddress::yaml(SourceLocator::path(SOURCE_ALIAS).unwrap(), "1").unwrap()
}

fn name_path() -> FieldPath {
    FieldPath::root().push_field("m_Name").unwrap()
}

fn guard(value: &str) -> FieldGuard {
    let class = unity_asset::UnityClass::new(1, "GameObject".to_owned(), "1".to_owned());
    let path = name_path();
    let value = UnityValue::String(value.to_owned());
    let mut budget = AssetLoadBudget::default();
    FieldGuard::new(
        yaml_field_schema_digest(&class, &path, &value, &mut budget).unwrap(),
        semantic_value_digest(&value, &mut budget).unwrap(),
    )
}

fn plan(workspace: &AssetWorkspace, bytes: &[u8], before: &str, after: &str) -> MutationPlan {
    MutationPlan::new(
        workspace.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(SOURCE_ALIAS).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, bytes),
        )],
        Vec::new(),
        vec![GenericMutation::FieldReplace {
            target: address(),
            path: name_path(),
            guard: guard(before),
            replacement: MutationValue::string(after).unwrap(),
        }],
    )
    .unwrap()
}

fn read_name(view: &impl WorkspaceView) -> String {
    let mut budget = AssetLoadBudget::default();
    let WorkspaceLookup::Resolved(handle) = view.resolve_object(&address(), &mut budget).unwrap()
    else {
        panic!("fixture object must resolve");
    };
    view.read_object(&handle, &mut budget)
        .unwrap()
        .class()
        .value_at_path(&name_path())
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

fn target(root: &Path) -> PublicationTarget {
    PublicationTarget::in_place(root).unwrap()
}

fn manifest_count(root: &Path) -> usize {
    let version = root.join(".unity-asset-recovery").join("v1");
    fs::read_dir(version)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("manifest.v1.json").is_file())
        .count()
}

#[test]
fn commit_publishes_exact_bytes_and_installs_the_new_baseline() {
    let (directory, path, mut workspace) = fixture();
    let base = workspace.snapshot();
    let prepared = workspace
        .prepare(
            plan(&workspace, YAML.as_bytes(), "Before", "After"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let prepared_view = prepared.view();
    let prepared_revision = prepared_view.revision();

    let report = workspace
        .commit(
            prepared,
            target(directory.path()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(report.base_revision(), base.revision());
    assert_eq!(report.committed_revision(), prepared_revision);
    assert_eq!(report.changes().changed_objects().len(), 1);
    let WorkspaceLookup::Resolved(expected_object) = prepared_view
        .resolve_object(&address(), &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("prepared object must resolve");
    };
    assert_eq!(
        report.changes().changed_objects()[0],
        expected_object.object().clone()
    );
    assert!(report.changes().identity_remaps().is_empty());
    assert_eq!(workspace.revision(), prepared_revision);
    assert_eq!(read_name(&base), "Before");
    assert_eq!(read_name(&prepared_view), "After");
    assert_eq!(read_name(&workspace.snapshot()), "After");
    assert!(
        String::from_utf8(fs::read(path).unwrap())
            .unwrap()
            .contains("m_Name: After")
    );
    assert!(report.recovery().root().join("manifest.v1.json").is_file());
}

#[test]
fn only_one_change_from_the_same_baseline_can_commit() {
    let (directory, path, mut workspace) = fixture();
    let first = workspace
        .prepare(
            plan(&workspace, YAML.as_bytes(), "Before", "First"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let second = workspace
        .prepare(
            plan(&workspace, YAML.as_bytes(), "Before", "Second"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    workspace
        .commit(
            first,
            target(directory.path()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let committed = fs::read(&path).unwrap();
    let error = workspace
        .commit(
            second,
            target(directory.path()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert!(matches!(error, CommitError::StaleRevision { .. }));
    assert!(error.prepared().is_none());
    assert!(error.into_prepared().is_none());
    assert_eq!(fs::read(path).unwrap(), committed);
    assert_eq!(read_name(&workspace.snapshot()), "First");
}

#[test]
fn a_second_commit_starts_from_the_first_committed_bytes() {
    let (directory, path, mut workspace) = fixture();
    let first = workspace
        .prepare(
            plan(&workspace, YAML.as_bytes(), "Before", "First"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    workspace
        .commit(
            first,
            target(directory.path()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let first_bytes = fs::read(&path).unwrap();
    let second = workspace
        .prepare(
            plan(&workspace, &first_bytes, "First", "Second"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    workspace
        .commit(
            second,
            target(directory.path()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(read_name(&workspace.snapshot()), "Second");
    assert!(
        String::from_utf8(fs::read(path).unwrap())
            .unwrap()
            .contains("m_Name: Second")
    );
}

#[test]
fn commit_budget_failure_returns_the_prepared_change_without_publishing() {
    let (directory, path, mut workspace) = fixture();
    let prepared = workspace
        .prepare(
            plan(&workspace, YAML.as_bytes(), "Before", "After"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let prepared_revision = prepared.report().prepared_revision();
    let original = fs::read(&path).unwrap();
    let mut tiny = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    let error = workspace
        .commit(prepared, target(directory.path()), &mut tiny)
        .unwrap_err();
    let CommitError::Budget {
        source: BudgetError::Exceeded {
            resource: "bytes", ..
        },
        prepared,
    } = error
    else {
        panic!("commit must preserve a typed byte-budget failure");
    };

    assert_eq!(prepared.report().prepared_revision(), prepared_revision);
    assert_eq!(
        workspace.snapshot().revision(),
        prepared.report().base_revision()
    );
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn commit_obeys_exact_and_one_short_byte_budgets_before_manifest_installation() {
    let (directory, path, mut workspace) = fixture();
    let warmup = workspace
        .prepare(
            plan(&workspace, YAML.as_bytes(), "Before", "Warmup_"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    workspace
        .commit(
            warmup,
            target(directory.path()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let warmup_bytes = fs::read(&path).unwrap();
    let measured = workspace
        .prepare(
            plan(&workspace, &warmup_bytes, "Warmup_", "Measure"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut measured_budget = AssetLoadBudget::default();
    workspace
        .commit(measured, target(directory.path()), &mut measured_budget)
        .unwrap();
    let usage = measured_budget.usage();
    assert!(usage.bytes > 0);
    let exact_limits = AssetLoadLimits {
        max_entries: usage.entries,
        max_bytes: usage.bytes,
        max_depth: usage.max_observed_depth,
        max_members: usage.members,
        ..AssetLoadLimits::default()
    };

    let measured_bytes = fs::read(&path).unwrap();
    let exact_prepared = workspace
        .prepare(
            plan(&workspace, &measured_bytes, "Measure", "Exact__"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
    workspace
        .commit(exact_prepared, target(directory.path()), &mut exact)
        .unwrap();
    assert_eq!(exact.usage(), usage);

    let exact_bytes = fs::read(&path).unwrap();
    let one_short_prepared = workspace
        .prepare(
            plan(&workspace, &exact_bytes, "Exact__", "Short__"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let base_revision = one_short_prepared.report().base_revision();
    let prepared_revision = one_short_prepared.report().prepared_revision();
    let manifests_before = manifest_count(directory.path());
    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: usage.bytes - 1,
        ..exact_limits
    })
    .unwrap();

    let error = workspace
        .commit(one_short_prepared, target(directory.path()), &mut one_short)
        .unwrap_err();
    let CommitError::Budget { prepared, .. } = error else {
        panic!("one-short commit must return a typed budget error");
    };

    assert_eq!(prepared.report().prepared_revision(), prepared_revision);
    assert_eq!(workspace.revision(), base_revision);
    assert_eq!(fs::read(path).unwrap(), exact_bytes);
    assert_eq!(manifest_count(directory.path()), manifests_before);
}

#[test]
fn source_conflict_consumes_the_prepared_change_without_overwriting_external_bytes() {
    let (directory, path, mut workspace) = fixture();
    let prepared = workspace
        .prepare(
            plan(&workspace, YAML.as_bytes(), "Before", "After"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let base_revision = prepared.report().base_revision();
    let external = YAML.replace("Before", "External");
    fs::write(&path, &external).unwrap();

    let error = workspace
        .commit(
            prepared,
            target(directory.path()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert!(matches!(error, CommitError::SourceConflict { .. }));
    assert!(error.prepared().is_none());
    assert!(error.into_prepared().is_none());
    assert_eq!(workspace.revision(), base_revision);
    assert_eq!(fs::read(path).unwrap(), external.as_bytes());
}

#[test]
fn hardlinked_target_is_terminally_rejected_before_journal_publication() {
    let (directory, path, mut workspace) = fixture();
    let prepared = workspace
        .prepare(
            plan(&workspace, YAML.as_bytes(), "Before", "After"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let base_revision = prepared.report().base_revision();
    let alias = directory.path().join("external-hardlink.prefab");
    fs::hard_link(&path, &alias).unwrap();

    let error = workspace
        .commit(
            prepared,
            target(directory.path()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert!(matches!(error, CommitError::PublishBlocked { .. }));
    assert!(error.prepared().is_none());
    assert!(error.into_prepared().is_none());
    assert_eq!(workspace.revision(), base_revision);
    assert_eq!(fs::read(&path).unwrap(), YAML.as_bytes());
    assert_eq!(fs::read(alias).unwrap(), YAML.as_bytes());
    let version = directory.path().join(".unity-asset-recovery").join("v1");
    let entries: Vec<_> = fs::read_dir(version)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, [std::ffi::OsString::from(".commit.lock")]);
}

#[test]
fn recovery_baseline_entry_budget_failure_remains_typed_and_prejournal() {
    let (measured_directory, _measured_path, mut measured_workspace) = fixture();
    let measured_prepared = measured_workspace
        .prepare(
            plan(&measured_workspace, YAML.as_bytes(), "Before", "After"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut measured_budget = AssetLoadBudget::default();
    measured_workspace
        .commit(
            measured_prepared,
            target(measured_directory.path()),
            &mut measured_budget,
        )
        .unwrap();
    let required_entries = measured_budget.usage().entries;
    assert!(required_entries > 0);

    let (directory, path, mut workspace) = fixture();
    let prepared = workspace
        .prepare(
            plan(&workspace, YAML.as_bytes(), "Before", "After"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let base_revision = prepared.report().base_revision();
    let prepared_revision = prepared.report().prepared_revision();
    let original = fs::read(&path).unwrap();
    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: required_entries - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    let error = workspace
        .commit(prepared, target(directory.path()), &mut one_short)
        .unwrap_err();
    let CommitError::Budget {
        source:
            BudgetError::Exceeded {
                resource: "entries",
                limit,
                requested,
            },
        prepared,
    } = error
    else {
        panic!("recovery baseline exhaustion must remain a typed entry-budget error");
    };

    assert_eq!(limit, required_entries - 1);
    assert_eq!(requested, required_entries);
    assert!(one_short.usage().entries < limit);
    assert_eq!(prepared.report().prepared_revision(), prepared_revision);
    assert_eq!(workspace.revision(), base_revision);
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(manifest_count(directory.path()), 0);
}
