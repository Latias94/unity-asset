use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use unity_asset::schema::{AudioClipResourceRecipe, SchemaRecipePlanner};
use unity_asset::workspace::{
    AssetWorkspace, FieldGuard, GenericMutation, MutationPlan, MutationPlanBuilder, MutationValue,
    PlanPayload, PrepareOptions, SourceExpectation, WorkspaceLookup, WorkspaceView,
};
use unity_asset::{
    AssetLoadBudget, AssetLoadLimits, ContainmentKind, DigestV1, FieldPath, ObjectAddress,
    SourceFingerprint, SourceKind, SourceLocator, SourceMemberId, UnityValue,
};
use unity_asset_core::{semantic_value_digest, yaml_field_schema_digest};
use unity_asset_write::artifact::ArtifactLimits;

const ALIAS: &str = "audio-clip.asset";
const PAYLOAD: &[u8] = b"OggS-prepared-audio";
const VALID_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!83 &8300001
AudioClip:
  m_StreamData: {path: archive:/CAB-old/CAB-old.resS, offset: 7, size: 4}
"#;
const ORDERED_FAILURE_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!83 &8300001
AudioClip:
  m_Name: Clip
  m_StreamData: {path: archive:/CAB-old/CAB-old.resS, offset: 7, size: 4}
"#;
const INVALID_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!83 &8300001
AudioClip:
  m_StreamData: invalid
"#;
const MISSING_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!83 &8300001
AudioClip:
  m_AudioData: T2dnUw==
"#;

struct Fixture {
    directory: TempDir,
    source_path: PathBuf,
    workspace: AssetWorkspace,
    yaml: &'static str,
}

impl Fixture {
    fn open(yaml: &'static str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join(ALIAS);
        fs::write(&source_path, yaml).unwrap();
        let mut workspace = AssetWorkspace::new().unwrap();
        workspace
            .load_path(&source_path, &mut AssetLoadBudget::default())
            .unwrap();
        Self {
            directory,
            source_path,
            workspace,
            yaml,
        }
    }

    fn address(&self) -> ObjectAddress {
        ObjectAddress::yaml(SourceLocator::path(ALIAS).unwrap(), "8300001").unwrap()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TreeEntry {
    Directory(PathBuf),
    File(PathBuf, Vec<u8>),
}

fn snapshot_tree(root: &Path) -> Vec<TreeEntry> {
    fn visit(root: &Path, current: &Path, entries: &mut Vec<TreeEntry>) {
        let mut children = fs::read_dir(current)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        children.sort();
        for path in children {
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if path.is_dir() {
                entries.push(TreeEntry::Directory(relative));
                visit(root, &path, entries);
            } else {
                entries.push(TreeEntry::File(relative, fs::read(&path).unwrap()));
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries
}

fn valid_recipe_plan(fixture: &Fixture) -> MutationPlan {
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let object = planner
        .inspect(&fixture.address(), &mut AssetLoadBudget::default())
        .unwrap();
    let lowering = AudioClipResourceRecipe::lower(
        &planner,
        &object,
        PlanPayload::new(PAYLOAD.to_vec()),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let mut builder = MutationPlanBuilder::new(snapshot.workspace_id(), snapshot.revision());
    builder.append(lowering.into_fragment().unwrap()).unwrap();
    builder.build().unwrap()
}

fn direct_resource_plan(fixture: &Fixture) -> MutationPlan {
    let snapshot = fixture.workspace.snapshot();
    let path = FieldPath::root().push_field("m_StreamData").unwrap();
    let guard = observed_resource_guard(fixture, &path);
    let payload = PlanPayload::new(PAYLOAD.to_vec());
    MutationPlan::new(
        snapshot.workspace_id(),
        snapshot.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(ALIAS).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, fixture.yaml.as_bytes()),
        )],
        vec![payload.clone()],
        vec![GenericMutation::ResourceReplace {
            target: fixture.address(),
            path,
            guard,
            payload: payload.digest(),
        }],
    )
    .unwrap()
}

fn observed_resource_guard(fixture: &Fixture, path: &FieldPath) -> FieldGuard {
    let snapshot = fixture.workspace.snapshot();
    let WorkspaceLookup::Resolved(handle) = snapshot
        .resolve_object(&fixture.address(), &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("AudioClip fixture must resolve");
    };
    let object = snapshot
        .read_object(&handle, &mut AssetLoadBudget::default())
        .unwrap();
    match object.class().value_at_path(path) {
        Ok(current) => FieldGuard::new(
            yaml_field_schema_digest(
                object.class(),
                path,
                current,
                &mut AssetLoadBudget::default(),
            )
            .unwrap(),
            semantic_value_digest(current, &mut AssetLoadBudget::default()).unwrap(),
        ),
        Err(_) => FieldGuard::new(
            DigestV1::hash_bytes(b"missing-resource-schema"),
            DigestV1::hash_bytes(b"missing-resource-value"),
        ),
    }
}

fn resource_value(view: &impl WorkspaceView, fixture: &Fixture) -> UnityValue {
    let WorkspaceLookup::Resolved(handle) = view
        .resolve_object(&fixture.address(), &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("AudioClip fixture must resolve");
    };
    view.read_object(&handle, &mut AssetLoadBudget::default())
        .unwrap()
        .class()
        .value_at_path(&FieldPath::root().push_field("m_StreamData").unwrap())
        .unwrap()
        .clone()
}

#[test]
fn audio_clip_stream_data_prepare_adds_one_exact_sidecar_without_writing_disk() {
    let fixture = Fixture::open(VALID_YAML);
    let before = snapshot_tree(fixture.directory.path());
    let prepared = fixture
        .workspace
        .prepare(
            valid_recipe_plan(&fixture),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(snapshot_tree(fixture.directory.path()), before);
    assert_eq!(
        fs::read(&fixture.source_path).unwrap(),
        VALID_YAML.as_bytes()
    );

    let prepared_value = resource_value(&prepared.view(), &fixture);
    let fields = prepared_value.as_object().unwrap();
    let path = fields.get("path").and_then(UnityValue::as_str).unwrap();
    assert_eq!(fields.get("offset").and_then(UnityValue::as_u64), Some(0));
    assert_eq!(
        fields.get("size").and_then(UnityValue::as_u64),
        Some(PAYLOAD.len() as u64)
    );

    let baseline = resource_value(&fixture.workspace.snapshot(), &fixture);
    assert_eq!(
        baseline
            .as_object()
            .unwrap()
            .get("path")
            .and_then(UnityValue::as_str),
        Some("archive:/CAB-old/CAB-old.resS")
    );

    let sources = prepared
        .view()
        .sources(&mut AssetLoadBudget::default())
        .unwrap();
    let resources = sources
        .iter()
        .filter(|source| source.kind() == SourceKind::StreamedResource)
        .collect::<Vec<_>>();
    assert_eq!(resources.len(), 1);
    let resource = resources[0];
    let member = resource.locator().members().last().unwrap();
    assert_eq!(member.container(), ContainmentKind::Companion);
    assert_eq!(member.name(), path);

    let range = prepared
        .view()
        .read_source_range(
            resource.id(),
            0,
            PAYLOAD.len() as u64,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut actual = Vec::new();
    range.reader().read_to_end(&mut actual).unwrap();
    assert_eq!(actual, PAYLOAD);

    let resource_reports = prepared
        .report()
        .sources()
        .iter()
        .filter(|source| source.source_id().kind() == SourceKind::StreamedResource)
        .collect::<Vec<_>>();
    assert_eq!(resource_reports.len(), 1);
    assert_eq!(resource_reports[0].base_fingerprint(), None);
    assert_eq!(resource_reports[0].artifact_bytes(), PAYLOAD.len() as u64);
    assert!(resource_reports[0].publication_root());
}

#[test]
fn invalid_or_missing_resource_fields_leave_no_sidecar_or_candidate() {
    for yaml in [INVALID_YAML, MISSING_YAML] {
        let fixture = Fixture::open(yaml);
        let before = snapshot_tree(fixture.directory.path());
        let error = fixture
            .workspace
            .prepare(
                direct_resource_plan(&fixture),
                PrepareOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert_eq!(snapshot_tree(fixture.directory.path()), before);
        assert_eq!(fs::read(&fixture.source_path).unwrap(), yaml.as_bytes());
        assert_eq!(error.report().diagnostics().len(), 1);
        assert_eq!(error.report().diagnostics()[0].ordinal(), Some(0));
        assert!(
            fixture
                .workspace
                .snapshot()
                .sources(&mut AssetLoadBudget::default())
                .unwrap()
                .iter()
                .all(|source| source.kind() != SourceKind::StreamedResource)
        );
    }
}

#[test]
fn earlier_field_guard_failure_precedes_later_missing_resource_target() {
    let fixture = Fixture::open(ORDERED_FAILURE_YAML);
    let snapshot = fixture.workspace.snapshot();
    let field_path = FieldPath::root().push_field("m_Name").unwrap();
    let resource_path = FieldPath::root().push_field("m_StreamData").unwrap();
    let payload = PlanPayload::new(PAYLOAD.to_vec());
    let missing = ObjectAddress::yaml(SourceLocator::path(ALIAS).unwrap(), "8300002").unwrap();
    let plan = MutationPlan::new(
        snapshot.workspace_id(),
        snapshot.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(ALIAS).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, ORDERED_FAILURE_YAML.as_bytes()),
        )],
        vec![payload.clone()],
        vec![
            GenericMutation::FieldReplace {
                target: fixture.address(),
                path: field_path,
                guard: FieldGuard::new(
                    DigestV1::hash_bytes(b"stale-field-schema"),
                    DigestV1::hash_bytes(b"stale-field-value"),
                ),
                replacement: MutationValue::string("never-applied").unwrap(),
            },
            GenericMutation::ResourceReplace {
                target: missing,
                path: resource_path,
                guard: FieldGuard::new(
                    DigestV1::hash_bytes(b"unreachable-resource-schema"),
                    DigestV1::hash_bytes(b"unreachable-resource-value"),
                ),
                payload: payload.digest(),
            },
        ],
    )
    .unwrap();

    let error = fixture
        .workspace
        .prepare(
            plan,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert_eq!(error.report().diagnostics().len(), 1);
    let diagnostic = &error.report().diagnostics()[0];
    assert_eq!(diagnostic.ordinal(), Some(0));
    assert_eq!(diagnostic.diagnostic().code(), "PREPARE_MUTATION_REJECTED");
}

#[test]
fn source_expectations_are_global_preconditions_before_resource_guards() {
    let fixture = Fixture::open(VALID_YAML);
    let snapshot = fixture.workspace.snapshot();
    let path = FieldPath::root().push_field("m_StreamData").unwrap();
    let payload = PlanPayload::new(PAYLOAD.to_vec());
    let missing_locator = SourceLocator::path(ALIAS)
        .unwrap()
        .child(
            ContainmentKind::Companion,
            SourceMemberId::new("missing.asset").unwrap(),
        )
        .unwrap();
    let missing = ObjectAddress::yaml(missing_locator.clone(), "1").unwrap();
    let stale = FieldGuard::new(
        DigestV1::hash_bytes(b"stale-resource-schema"),
        DigestV1::hash_bytes(b"stale-resource-value"),
    );
    let plan = MutationPlan::new(
        snapshot.workspace_id(),
        snapshot.revision(),
        vec![
            SourceExpectation::new(
                SourceLocator::path(ALIAS).unwrap(),
                SourceFingerprint::from_bytes(SourceKind::Yaml, VALID_YAML.as_bytes()),
            ),
            SourceExpectation::new(
                missing_locator,
                SourceFingerprint::from_bytes(SourceKind::Yaml, b"missing"),
            ),
        ],
        vec![payload.clone()],
        vec![
            GenericMutation::ResourceReplace {
                target: fixture.address(),
                path: path.clone(),
                guard: stale,
                payload: payload.digest(),
            },
            GenericMutation::ResourceReplace {
                target: missing,
                path,
                guard: stale,
                payload: payload.digest(),
            },
        ],
    )
    .unwrap();

    let error = fixture
        .workspace
        .prepare(
            plan,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    let diagnostic = &error.report().diagnostics()[0];
    assert_eq!(diagnostic.ordinal(), None);
    assert_eq!(diagnostic.diagnostic().code(), "PREPARE_SOURCE_MISSING");
}

#[test]
fn current_resource_guard_precedes_a_future_same_domain_manifest_budget() {
    let fixture = Fixture::open(VALID_YAML);
    let snapshot = fixture.workspace.snapshot();
    let path = FieldPath::root().push_field("m_StreamData").unwrap();
    let first_payload = PlanPayload::new(PAYLOAD.to_vec());
    let future_payload = PlanPayload::new(vec![0x5a; 1024 * 1024]);
    let stale = FieldGuard::new(
        DigestV1::hash_bytes(b"stale-resource-schema"),
        DigestV1::hash_bytes(b"stale-resource-value"),
    );
    let plan = MutationPlan::new(
        snapshot.workspace_id(),
        snapshot.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(ALIAS).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, VALID_YAML.as_bytes()),
        )],
        vec![first_payload.clone(), future_payload.clone()],
        vec![
            GenericMutation::ResourceReplace {
                target: fixture.address(),
                path: path.clone(),
                guard: stale,
                payload: first_payload.digest(),
            },
            GenericMutation::ResourceReplace {
                target: fixture.address(),
                path,
                guard: stale,
                payload: future_payload.digest(),
            },
        ],
    )
    .unwrap();
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: 512 * 1024,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    let error = fixture
        .workspace
        .prepare(plan, PrepareOptions::default(), &mut budget)
        .unwrap_err();
    let diagnostic = &error.report().diagnostics()[0];
    assert_eq!(diagnostic.ordinal(), Some(0));
    assert_eq!(
        diagnostic.diagnostic().code(),
        "PREPARE_RESOURCE_GUARD_REJECTED"
    );
}

#[test]
fn later_resource_guard_precedes_its_large_payload_budget() {
    let fixture = Fixture::open(VALID_YAML);
    let snapshot = fixture.workspace.snapshot();
    let path = FieldPath::root().push_field("m_StreamData").unwrap();
    let first_payload = PlanPayload::new(PAYLOAD.to_vec());
    let second_payload = PlanPayload::new(vec![0x5a; 1024 * 1024]);
    let original_guard = observed_resource_guard(&fixture, &path);
    let plan = MutationPlan::new(
        snapshot.workspace_id(),
        snapshot.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(ALIAS).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, VALID_YAML.as_bytes()),
        )],
        vec![first_payload.clone(), second_payload.clone()],
        vec![
            GenericMutation::ResourceReplace {
                target: fixture.address(),
                path: path.clone(),
                guard: original_guard,
                payload: first_payload.digest(),
            },
            GenericMutation::ResourceReplace {
                target: fixture.address(),
                path,
                guard: original_guard,
                payload: second_payload.digest(),
            },
        ],
    )
    .unwrap();
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: 512 * 1024,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    let error = fixture
        .workspace
        .prepare(plan, PrepareOptions::default(), &mut budget)
        .unwrap_err();
    let diagnostic = &error.report().diagnostics()[0];
    assert_eq!(diagnostic.ordinal(), Some(1));
    assert_eq!(
        diagnostic.diagnostic().code(),
        "PREPARE_RESOURCE_GUARD_REJECTED"
    );
}

#[test]
fn resource_artifact_budget_failure_preserves_the_complete_directory_tree() {
    let fixture = Fixture::open(VALID_YAML);
    let before = snapshot_tree(fixture.directory.path());
    let options = PrepareOptions::new(ArtifactLimits::default().with_max_outputs(1));
    let error = fixture
        .workspace
        .prepare(
            valid_recipe_plan(&fixture),
            options,
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert_eq!(snapshot_tree(fixture.directory.path()), before);
    assert_eq!(
        fs::read(&fixture.source_path).unwrap(),
        VALID_YAML.as_bytes()
    );
    assert!(
        error
            .report()
            .diagnostics()
            .iter()
            .all(|diagnostic| diagnostic.diagnostic().code().starts_with("PREPARE_"))
    );
}
