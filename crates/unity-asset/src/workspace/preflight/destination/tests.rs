use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, SourceFingerprint, SourceKind,
};
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactLimits, LogicalArtifactName,
    PreparedArtifactSet,
};

use super::{
    DestinationExpectation, DestinationProofError, DestinationProofSet, DestinationState,
    PublicationDestination,
};

fn artifacts(names: &[&str]) -> PreparedArtifactSet {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let slots = names
        .iter()
        .map(|name| {
            declaration
                .declare_output(LogicalArtifactName::new(name).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let mut batch = declaration.seal_output_names().unwrap();
    for (ordinal, slot) in slots.into_iter().enumerate() {
        let mut writer = batch.yaml_writer().unwrap();
        writeln!(writer, "---\nvalue: {ordinal}").unwrap();
        let artifact = batch.prepare_yaml_writer(writer).unwrap();
        batch.bind_output(slot, artifact).unwrap();
    }
    batch.finish().unwrap()
}

fn declarations_under_root<'a>(
    artifacts: &'a PreparedArtifactSet,
    root: &'a Path,
) -> Vec<PublicationDestination<'a>> {
    artifacts
        .outputs()
        .map(|output| {
            PublicationDestination::under_root(output.name(), root, DestinationExpectation::Observe)
        })
        .collect()
}

fn tree_entries(root: &Path) -> Vec<PathBuf> {
    fn collect(root: &Path, current: &Path, entries: &mut Vec<PathBuf>) {
        let mut children = fs::read_dir(current)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        children.sort();
        for path in children {
            entries.push(path.strip_prefix(root).unwrap().to_path_buf());
            if path.is_dir() {
                collect(root, &path, entries);
            }
        }
    }

    let mut entries = Vec::new();
    collect(root, root, &mut entries);
    entries
}

#[test]
fn observes_existing_and_nested_absent_outputs_in_canonical_order() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("nested")).unwrap();
    let existing_path = directory.path().join("z-existing.yaml");
    let existing_bytes = b"old destination";
    fs::write(&existing_path, existing_bytes).unwrap();
    let artifacts = artifacts(&["z-existing.yaml", "nested/a-new.yaml"]);
    let outputs = artifacts.outputs().collect::<Vec<_>>();
    let declarations = vec![
        PublicationDestination::under_root(
            outputs[0].name(),
            directory.path(),
            DestinationExpectation::Observe,
        ),
        PublicationDestination::under_root(
            outputs[1].name(),
            directory.path(),
            DestinationExpectation::Observe,
        ),
    ];

    let proof =
        DestinationProofSet::observe(&artifacts, &declarations, &mut AssetLoadBudget::default())
            .unwrap();

    assert_eq!(proof.bindings()[0].output_name(), "nested/a-new.yaml");
    assert_eq!(proof.bindings()[0].expected(), DestinationState::Absent);
    assert_eq!(proof.bindings()[1].output_name(), "z-existing.yaml");
    assert_eq!(
        proof.bindings()[1].expected(),
        DestinationState::Existing(SourceFingerprint::from_bytes(
            SourceKind::Yaml,
            existing_bytes,
        ))
    );
    assert_eq!(
        proof.bindings()[1].target(),
        fs::canonicalize(existing_path).unwrap()
    );
    assert!(proof.revalidate(&mut AssetLoadBudget::default()).is_ok());
}

#[test]
fn existing_content_change_reports_expected_and_actual_fingerprints() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("main.yaml");
    fs::write(&target, b"old-bytes").unwrap();
    let artifacts = artifacts(&["main.yaml"]);
    let declarations = declarations_under_root(&artifacts, directory.path());
    let proof =
        DestinationProofSet::observe(&artifacts, &declarations, &mut AssetLoadBudget::default())
            .unwrap();

    fs::write(&target, b"new-bytes").unwrap();
    let expected = SourceFingerprint::from_bytes(SourceKind::Yaml, b"old-bytes");
    let actual = SourceFingerprint::from_bytes(SourceKind::Yaml, b"new-bytes");

    assert!(matches!(
        proof.revalidate(&mut AssetLoadBudget::default()),
        Err(DestinationProofError::ObservationMismatch {
            output: 0,
            expected: DestinationState::Existing(found_expected),
            actual: DestinationState::Existing(found_actual),
        }) if found_expected == expected && found_actual == actual
    ));
}

#[test]
fn absent_target_appearance_is_a_typed_conflict() {
    let directory = tempfile::tempdir().unwrap();
    let artifacts = artifacts(&["new.yaml"]);
    let declarations = declarations_under_root(&artifacts, directory.path());
    let proof =
        DestinationProofSet::observe(&artifacts, &declarations, &mut AssetLoadBudget::default())
            .unwrap();

    fs::write(directory.path().join("new.yaml"), b"external").unwrap();

    assert!(matches!(
        proof.revalidate(&mut AssetLoadBudget::default()),
        Err(DestinationProofError::ObservationMismatch {
            output: 0,
            expected: DestinationState::Absent,
            actual: DestinationState::Existing(actual),
        }) if actual == SourceFingerprint::from_bytes(SourceKind::Yaml, b"external")
    ));
}

#[test]
fn rejects_duplicate_targets_and_non_bijective_output_mappings() {
    let directory = tempfile::tempdir().unwrap();
    let artifacts = artifacts(&["a.yaml", "b.yaml"]);
    let outputs = artifacts.outputs().collect::<Vec<_>>();
    let shared_target = directory.path().join("shared.yaml");
    let duplicates = vec![
        PublicationDestination::exact(
            outputs[0].name(),
            &shared_target,
            DestinationExpectation::Absent,
        ),
        PublicationDestination::exact(
            outputs[1].name(),
            &shared_target,
            DestinationExpectation::Absent,
        ),
    ];
    assert!(matches!(
        DestinationProofSet::observe(&artifacts, &duplicates, &mut AssetLoadBudget::default(),),
        Err(DestinationProofError::DuplicateTarget {
            first_output: 0,
            second_output: 1,
        })
    ));

    let missing = [PublicationDestination::under_root(
        outputs[0].name(),
        directory.path(),
        DestinationExpectation::Observe,
    )];
    assert!(matches!(
        DestinationProofSet::observe(&artifacts, &missing, &mut AssetLoadBudget::default(),),
        Err(DestinationProofError::OutputCountMismatch {
            outputs: 2,
            destinations: 1,
        })
    ));

    let unknown_name = LogicalArtifactName::new("unknown.yaml").unwrap();
    let wrong = vec![
        PublicationDestination::under_root(
            &unknown_name,
            directory.path(),
            DestinationExpectation::Observe,
        ),
        PublicationDestination::under_root(
            outputs[0].name(),
            directory.path(),
            DestinationExpectation::Observe,
        ),
    ];
    assert!(matches!(
        DestinationProofSet::observe(&artifacts, &wrong, &mut AssetLoadBudget::default(),),
        Err(DestinationProofError::OutputNameMismatch {
            output: 1,
            destination: 0,
        })
    ));
}

#[test]
fn rejects_targets_that_alias_under_portable_filesystem_rules() {
    let directory = tempfile::tempdir().unwrap();
    let artifacts = artifacts(&["a.yaml", "b.yaml"]);
    let outputs = artifacts.outputs().collect::<Vec<_>>();

    for (first_name, second_name) in [
        ("Foo.asset", "foo.asset"),
        ("\u{e9}.asset", "e\u{301}.asset"),
    ] {
        let first = directory.path().join(first_name);
        let second = directory.path().join(second_name);
        let declarations = [
            PublicationDestination::exact(
                outputs[0].name(),
                &first,
                DestinationExpectation::Absent,
            ),
            PublicationDestination::exact(
                outputs[1].name(),
                &second,
                DestinationExpectation::Absent,
            ),
        ];

        assert!(matches!(
            DestinationProofSet::observe(
                &artifacts,
                &declarations,
                &mut AssetLoadBudget::default(),
            ),
            Err(DestinationProofError::PortableTargetCollision {
                first_output: 0,
                second_output: 1,
            })
        ));
    }
}

#[test]
fn distinguishes_destination_declaration_indices_from_canonical_output_ordinals() {
    let directory = tempfile::tempdir().unwrap();
    let artifacts = artifacts(&["z.yaml", "a.yaml", "m.yaml"]);
    let outputs = artifacts.outputs().collect::<Vec<_>>();
    let shared_target = directory.path().join("shared.yaml");
    let middle_target = directory.path().join("middle.yaml");

    let duplicate_outputs = vec![
        PublicationDestination::exact(
            outputs[0].name(),
            &shared_target,
            DestinationExpectation::Absent,
        ),
        PublicationDestination::exact(
            outputs[1].name(),
            &middle_target,
            DestinationExpectation::Absent,
        ),
        PublicationDestination::exact(
            outputs[0].name(),
            &middle_target,
            DestinationExpectation::Absent,
        ),
    ];
    assert!(matches!(
        DestinationProofSet::observe(
            &artifacts,
            &duplicate_outputs,
            &mut AssetLoadBudget::default(),
        ),
        Err(DestinationProofError::DuplicateOutput {
            first_destination_declaration: 0,
            second_destination_declaration: 2,
        })
    ));

    let duplicate_targets = vec![
        PublicationDestination::exact(
            outputs[0].name(),
            &shared_target,
            DestinationExpectation::Absent,
        ),
        PublicationDestination::exact(
            outputs[2].name(),
            &middle_target,
            DestinationExpectation::Absent,
        ),
        PublicationDestination::exact(
            outputs[1].name(),
            &shared_target,
            DestinationExpectation::Absent,
        ),
    ];
    assert!(matches!(
        DestinationProofSet::observe(
            &artifacts,
            &duplicate_targets,
            &mut AssetLoadBudget::default(),
        ),
        Err(DestinationProofError::DuplicateTarget {
            first_output: 0,
            second_output: 2,
        })
    ));
}

#[test]
fn revalidation_uses_canonical_output_ordinals_after_scrambled_declarations() {
    let directory = tempfile::tempdir().unwrap();
    let a_path = directory.path().join("a.yaml");
    let z_path = directory.path().join("z.yaml");
    fs::write(&a_path, b"a-old").unwrap();
    fs::write(&z_path, b"z-old").unwrap();
    let artifacts = artifacts(&["z.yaml", "a.yaml"]);
    let outputs = artifacts.outputs().collect::<Vec<_>>();
    let declarations = vec![
        PublicationDestination::exact(outputs[0].name(), &z_path, DestinationExpectation::Observe),
        PublicationDestination::exact(outputs[1].name(), &a_path, DestinationExpectation::Observe),
    ];
    let proof =
        DestinationProofSet::observe(&artifacts, &declarations, &mut AssetLoadBudget::default())
            .unwrap();

    fs::write(&z_path, b"z-new").unwrap();

    assert!(matches!(
        proof.revalidate(&mut AssetLoadBudget::default()),
        Err(DestinationProofError::ObservationMismatch {
            output: 1,
            expected: DestinationState::Existing(_),
            actual: DestinationState::Existing(_),
        })
    ));
}

#[test]
fn prepare_observation_performs_no_filesystem_writes() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("missing/tree")).unwrap();
    fs::write(directory.path().join("existing.yaml"), b"existing").unwrap();
    let artifacts = artifacts(&["existing.yaml", "missing/tree/new.yaml"]);
    let declarations = declarations_under_root(&artifacts, directory.path());
    let before = tree_entries(directory.path());

    let proof =
        DestinationProofSet::observe(&artifacts, &declarations, &mut AssetLoadBudget::default())
            .unwrap();

    assert_eq!(tree_entries(directory.path()), before);
    assert_eq!(proof.bindings().len(), 2);
    assert!(!directory.path().join("missing").exists());
}

#[test]
fn observation_and_revalidation_are_exactly_budgeted() {
    let directory = tempfile::tempdir().unwrap();
    let artifacts = artifacts(&["budget.yaml"]);
    let declarations = declarations_under_root(&artifacts, directory.path());

    let mut probe = AssetLoadBudget::default();
    let proof = DestinationProofSet::observe(&artifacts, &declarations, &mut probe).unwrap();
    let observed = probe.usage();
    assert!(observed.bytes > 0);
    assert!(observed.entries > 0);

    let exact_limits = AssetLoadLimits {
        max_bytes: observed.bytes,
        max_entries: observed.entries,
        ..AssetLoadLimits::default()
    };
    let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
    DestinationProofSet::observe(&artifacts, &declarations, &mut exact).unwrap();
    assert_eq!(exact.usage().bytes, observed.bytes);
    assert_eq!(exact.usage().entries, observed.entries);

    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: observed.bytes - 1,
        max_entries: observed.entries,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        DestinationProofSet::observe(&artifacts, &declarations, &mut one_short),
        Err(DestinationProofError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }))
    ));

    let mut one_entry_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: observed.bytes,
        max_entries: observed.entries - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        DestinationProofSet::observe(&artifacts, &declarations, &mut one_entry_short),
        Err(DestinationProofError::Budget(BudgetError::Exceeded {
            resource: "entries",
            ..
        }))
    ));

    let mut revalidation_probe = AssetLoadBudget::default();
    proof.revalidate(&mut revalidation_probe).unwrap();
    let revalidated = revalidation_probe.usage();
    let mut revalidation_exact = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: revalidated.bytes,
        max_entries: revalidated.entries,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    proof.revalidate(&mut revalidation_exact).unwrap();
    assert_eq!(revalidation_exact.usage(), revalidated);

    let mut revalidation_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: revalidated.bytes - 1,
        max_entries: revalidated.entries,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        proof.revalidate(&mut revalidation_short),
        Err(DestinationProofError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }))
    ));
}

#[test]
fn budgeted_vec_charges_entries_and_rejects_one_short_before_allocating() {
    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: 3,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let values = super::budgeted_vec::<u64>(3, &mut exact).unwrap();
    assert!(values.capacity() >= 3);
    assert_eq!(exact.usage().entries, 3);

    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: 2,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        super::budgeted_vec::<u64>(3, &mut one_short),
        Err(DestinationProofError::Budget(BudgetError::Exceeded {
            resource: "entries",
            ..
        }))
    ));
    assert_eq!(one_short.usage().entries, 0);
    assert_eq!(one_short.usage().bytes, 0);
}

#[test]
fn byte_identical_path_replacement_fails_file_identity_cas() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("main.yaml");
    let displaced = directory.path().join("displaced.yaml");
    fs::write(&target, b"same bytes").unwrap();
    let artifacts = artifacts(&["main.yaml"]);
    let declarations = declarations_under_root(&artifacts, directory.path());
    let proof =
        DestinationProofSet::observe(&artifacts, &declarations, &mut AssetLoadBudget::default())
            .unwrap();

    fs::rename(&target, displaced).unwrap();
    fs::write(&target, b"same bytes").unwrap();

    assert!(matches!(
        proof.revalidate(&mut AssetLoadBudget::default()),
        Err(DestinationProofError::FileIdentityChanged {
            output: 0,
            expected_fingerprint,
        }) if expected_fingerprint == SourceFingerprint::from_bytes(SourceKind::Yaml, b"same bytes")
    ));
}

#[test]
fn symlink_replacement_is_never_treated_as_the_observed_file() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("main.yaml");
    let external = directory.path().join("external.yaml");
    fs::write(&target, b"same bytes").unwrap();
    fs::write(&external, b"same bytes").unwrap();
    let artifacts = artifacts(&["main.yaml"]);
    let declarations = declarations_under_root(&artifacts, directory.path());
    let proof =
        DestinationProofSet::observe(&artifacts, &declarations, &mut AssetLoadBudget::default())
            .unwrap();
    fs::remove_file(&target).unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink(&external, &target).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(&external, &target).is_err() {
        return;
    }

    assert!(matches!(
        proof.revalidate(&mut AssetLoadBudget::default()),
        Err(DestinationProofError::ObservationMismatch {
            output: 0,
            expected: DestinationState::Existing(_),
            actual: DestinationState::SymbolicLink,
        })
    ));
}
