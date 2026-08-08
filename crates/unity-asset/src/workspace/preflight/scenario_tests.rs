use std::fs;
use std::io::{Cursor, Write};
use std::sync::Arc;

use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use unity_asset_binary::bundle::{AssetBundle, BundleHeader, BundleParser};
use unity_asset_binary::compression::CompressionBlock;
use unity_asset_binary::webfile::WebFile;
use unity_asset_core::{
    ContainmentKind, SourceMemberId, VerifiedSourceImage, semantic_value_digest,
    yaml_field_schema_digest,
};
use unity_asset_write::PackingPolicy;
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactLimits, ArtifactPayload, LogicalArtifactName,
};
use unity_asset_write::bundle::{BundleArtifactEntry, BundleWriter};
use zip::CompressionMethod;
use zip::write::FileOptions;

use super::destination::{DestinationExpectation, DestinationProofSet, PublicationDestination};
use super::runner::PrepareCheckpoint;
use super::{
    PrepareFailureReport, PrepareOptions, PrepareReport, PreparedChange, PreparedLogicalChanges,
    PreparedPublicationError, PreparedPublicationSet,
};
use crate::reference::ReferenceFact;
use crate::workspace::{
    AssetWorkspace, CommitError, FieldGuard, GenericMutation, MutationPlan, MutationValue,
    PlanPayload, PublicationTarget, SourceExpectation, SourceOpenRequest, WorkspaceLookup,
    WorkspaceOptions, WorkspaceView,
};
use crate::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, DigestV1, FieldPath, ObjectAddress, ObjectId,
    SourceAlias, SourceFingerprint, SourceId, SourceKind, SourceLocator, UnityClass, UnityValue,
    WorkspaceId,
};

const TARGET_ALIAS: &str = "target.prefab";
const TARGET_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!114 &1
MonoBehaviour:
  m_Name: Before
  m_Target: {fileID: 2}
--- !u!1 &2
GameObject:
  m_Name: Target
"#;

const RESOURCE_ALIAS: &str = "resource.asset";
const RESOURCE_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!83 &1
AudioClip:
  m_StreamData:
    path: old.resS
    offset: 91
    size: 3
    untouched: true
"#;

fn fixture_source(bytes: &[u8]) -> ArtifactPayload {
    let workspace = WorkspaceId::from_u128(0x713).unwrap();
    let source = SourceId::new(workspace, SourceKind::Yaml, 1).unwrap();
    ArtifactPayload::source_backed(
        source,
        VerifiedSourceImage::verify(SourceKind::Yaml, Arc::<[u8]>::from(bytes)),
    )
    .unwrap()
}

fn fixture_bundle(name: &str, bytes: &[u8]) -> Vec<u8> {
    let header = BundleHeader {
        signature: "UnityFS".to_owned(),
        version: 7,
        unity_version: "2021.3.0f1".to_owned(),
        unity_revision: "2021.3.0f1".to_owned(),
        size: 1,
        compressed_blocks_info_size: 1,
        uncompressed_blocks_info_size: 1,
        flags: 0xc0,
        actual_header_size: 0,
        legacy_web_raw: None,
        file_stream_header_byte: None,
    };
    let mut bundle = AssetBundle::new(header, Vec::new());
    bundle.blocks.push(CompressionBlock::new(1, 1, 2));
    let payload = fixture_source(bytes);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration
        .declare_output(LogicalArtifactName::new("fixture.bundle").unwrap())
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let member = batch.prepare_verbatim_source(&payload).unwrap();
    let entries = [BundleArtifactEntry::file(&batch, name, 0, member).unwrap()];
    let root =
        BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, PackingPolicy::Uncompressed)
            .unwrap();
    batch.bind_output(output, root).unwrap();
    let artifacts = batch.finish().unwrap();
    let mut output_bytes = Vec::new();
    artifacts
        .artifact(root)
        .unwrap()
        .stream_verified_to(&mut output_bytes)
        .unwrap();
    output_bytes
}

fn fixture_webfile(name: &str, payload: &[u8]) -> Vec<u8> {
    let signature = b"UnityWebData1.0\0";
    let header_len = signature.len() + 4 + 12 + name.len();
    let mut bytes = signature.to_vec();
    bytes.extend_from_slice(&i32::try_from(header_len).unwrap().to_le_bytes());
    bytes.extend_from_slice(&i32::try_from(header_len).unwrap().to_le_bytes());
    bytes.extend_from_slice(&i32::try_from(payload.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(&i32::try_from(name.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn fixture_archive(name: &str, payload: &[u8]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = FileOptions::default().compression_method(CompressionMethod::Stored);
    writer.start_file(name, options).unwrap();
    writer.write_all(payload).unwrap();
    writer.finish().unwrap().into_inner()
}

fn container_locator(alias: &str, containment: ContainmentKind, member: &str) -> SourceLocator {
    SourceLocator::path(alias)
        .unwrap()
        .child(containment, SourceMemberId::new(member).unwrap())
        .unwrap()
}

fn artifact_output_bytes(prepared: &PreparedChange) -> Vec<u8> {
    assert_eq!(prepared.artifacts().outputs().len(), 1);
    let output = prepared.artifacts().outputs().next().unwrap();
    let mut bytes = Vec::new();
    output.artifact().stream_verified_to(&mut bytes).unwrap();
    bytes
}

fn prepared_resource_change() -> (tempfile::TempDir, AssetWorkspace, PreparedChange) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(RESOURCE_ALIAS);
    fs::write(&path, RESOURCE_YAML).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_source(
            SourceOpenRequest::new(&path, SourceAlias::new(RESOURCE_ALIAS).unwrap())
                .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let prepared = workspace
        .prepare(
            resource_plan(
                &workspace,
                SourceLocator::path(RESOURCE_ALIAS).unwrap(),
                RESOURCE_YAML.as_bytes(),
                b"replacement payload",
            ),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    (directory, workspace, prepared)
}

fn observe_prepared_destinations(prepared: &PreparedChange) -> DestinationProofSet {
    let destinations = prepared
        .publications()
        .iter()
        .map(|publication| {
            let output = prepared.artifacts().output(publication.output()).unwrap();
            PublicationDestination::exact(
                publication.source(),
                publication.output(),
                output.name(),
                publication.target(),
                DestinationExpectation::Observe,
            )
        })
        .collect::<Vec<_>>();
    DestinationProofSet::observe(
        prepared.artifacts(),
        &destinations,
        &mut AssetLoadBudget::default(),
    )
    .unwrap()
}

#[derive(Debug, PartialEq, Eq)]
struct PrepareOutcome {
    report: PrepareReport,
    artifact_usage: unity_asset_write::artifact::ArtifactBudgetUsage,
    outputs: Vec<(String, DigestV1, Vec<u8>)>,
    facts: Vec<ReferenceFact>,
}

fn name_path() -> FieldPath {
    FieldPath::root().push_field("m_Name").unwrap()
}

fn address() -> ObjectAddress {
    ObjectAddress::yaml(
        SourceLocator::path(TARGET_ALIAS).unwrap(),
        "1".parse().unwrap(),
    )
    .unwrap()
}

fn guard_for(value: &str) -> FieldGuard {
    let class = UnityClass::new(114, "MonoBehaviour".to_owned(), "1".to_owned());
    let value = UnityValue::String(value.to_owned());
    let mut budget = AssetLoadBudget::default();
    FieldGuard::new(
        yaml_field_schema_digest(&class, &name_path(), &value, &mut budget).unwrap(),
        semantic_value_digest(&value, &mut budget).unwrap(),
    )
}

fn replacement(expected: &str, replacement: &str) -> GenericMutation {
    GenericMutation::FieldReplace {
        target: address(),
        path: name_path(),
        guard: guard_for(expected),
        replacement: MutationValue::string(replacement).unwrap(),
    }
}

fn plan(workspace: &AssetWorkspace, actions: Vec<GenericMutation>) -> MutationPlan {
    MutationPlan::new(
        workspace.workspace_id(),
        workspace.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(TARGET_ALIAS).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, TARGET_YAML.as_bytes()),
        )],
        Vec::new(),
        actions,
    )
    .unwrap()
}

fn resource_path() -> FieldPath {
    FieldPath::root().push_field("m_StreamData").unwrap()
}

fn resource_address(locator: SourceLocator) -> ObjectAddress {
    ObjectAddress::yaml(locator, "1".parse().unwrap()).unwrap()
}

fn observed_field_guard(
    workspace: &AssetWorkspace,
    target: &ObjectAddress,
    path: &FieldPath,
) -> FieldGuard {
    let snapshot = workspace.snapshot();
    let WorkspaceLookup::Resolved(handle) = snapshot
        .resolve_object(target, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("resource fixture object must resolve");
    };
    let object = snapshot
        .read_object(&handle, &mut AssetLoadBudget::default())
        .unwrap();
    let value = object.class().value_at_path(path).unwrap();
    let mut budget = AssetLoadBudget::default();
    FieldGuard::new(
        yaml_field_schema_digest(object.class(), path, value, &mut budget).unwrap(),
        semantic_value_digest(value, &mut budget).unwrap(),
    )
}

fn resource_plan(
    workspace: &AssetWorkspace,
    locator: SourceLocator,
    source_bytes: &[u8],
    payload_bytes: &[u8],
) -> MutationPlan {
    let target = resource_address(locator.clone());
    let path = resource_path();
    let guard = observed_field_guard(workspace, &target, &path);
    let payload = PlanPayload::new(payload_bytes.to_vec());
    MutationPlan::new(
        workspace.workspace_id(),
        workspace.revision(),
        vec![SourceExpectation::new(
            locator,
            SourceFingerprint::from_bytes(SourceKind::Yaml, source_bytes),
        )],
        vec![payload.clone()],
        vec![GenericMutation::ResourceReplace {
            target,
            path,
            guard,
            payload: payload.digest(),
        }],
    )
    .unwrap()
}

fn workspace_with_order(
    workspace_id: WorkspaceId,
    paths: &[(String, std::path::PathBuf)],
    order: &[usize],
) -> AssetWorkspace {
    let mut workspace =
        AssetWorkspace::with_workspace_id(workspace_id, WorkspaceOptions::default()).unwrap();
    for index in order {
        let (alias, path) = &paths[*index];
        workspace
            .load_source(
                SourceOpenRequest::new(path, SourceAlias::new(alias).unwrap())
                    .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
    }
    workspace
}

fn outcome(prepared: &PreparedChange) -> PrepareOutcome {
    let outputs = prepared
        .artifacts()
        .outputs()
        .map(|output| {
            let mut bytes = Vec::new();
            output.artifact().stream_verified_to(&mut bytes).unwrap();
            (
                output.name().as_str().to_owned(),
                output.artifact().digest(),
                bytes,
            )
        })
        .collect();
    let facts = prepared.view().reference_graph().facts().to_vec();
    PrepareOutcome {
        report: prepared.report().clone(),
        artifact_usage: prepared.artifact_usage(),
        outputs,
        facts,
    }
}

fn assert_prepared_resource(
    prepared: &PreparedChange,
    locator: SourceLocator,
    payload: &[u8],
    expected_domain: SourceId,
    publication_root: bool,
) -> String {
    let sidecar = prepared
        .report()
        .sources()
        .iter()
        .find(|source| source.source_id().kind() == SourceKind::StreamedResource)
        .expect("resource prepare must report the generated sidecar");
    assert_eq!(sidecar.base_fingerprint(), None);
    assert_eq!(sidecar.artifact_bytes(), payload.len() as u64);
    assert_eq!(sidecar.logical_changed_bytes(), payload.len() as u64);
    assert_eq!(
        sidecar.physical_domain_owner(),
        if publication_root {
            sidecar.source_id()
        } else {
            expected_domain
        }
    );
    assert_eq!(sidecar.publication_root(), publication_root);
    let sidecar_name = sidecar
        .locator()
        .members()
        .last()
        .expect("generated sidecar locator must have a member")
        .member()
        .name()
        .to_owned();

    let target = resource_address(locator);
    let WorkspaceLookup::Resolved(handle) = prepared
        .view()
        .resolve_object(&target, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("prepared resource target must resolve");
    };
    let object = prepared
        .view()
        .read_object(&handle, &mut AssetLoadBudget::default())
        .unwrap();
    let fields = object
        .class()
        .value_at_path(&resource_path())
        .unwrap()
        .as_object()
        .unwrap();
    assert_eq!(fields["path"].as_str(), Some(sidecar_name.as_str()));
    assert_eq!(fields["offset"].as_u64(), Some(0));
    assert_eq!(fields["size"].as_u64(), Some(payload.len() as u64));
    assert_eq!(fields["untouched"].as_i64(), Some(1));

    let range = prepared
        .view()
        .read_source_range(
            sidecar.source_id(),
            0,
            payload.len() as u64,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut reparsed_payload = Vec::new();
    range.copy_to(&mut reparsed_payload).unwrap();
    assert_eq!(reparsed_payload, payload);
    sidecar_name
}

#[test]
fn prepared_publication_source_index_is_exactly_budgeted() {
    let (_directory, _workspace, prepared) = prepared_resource_change();
    let mut probe = AssetLoadBudget::default();
    PreparedPublicationSet::seal(
        observe_prepared_destinations(&prepared),
        prepared.state().as_ref(),
        &mut probe,
    )
    .unwrap();
    let usage = probe.usage();
    assert!(usage.bytes > 0);
    assert!(usage.entries > 0);
    assert!(usage.members > 0);

    let limits = AssetLoadLimits {
        max_bytes: usage.bytes,
        max_entries: usage.entries,
        max_members: usage.members,
        ..AssetLoadLimits::default()
    };
    let mut exact = AssetLoadBudget::new(limits).unwrap();
    PreparedPublicationSet::seal(
        observe_prepared_destinations(&prepared),
        prepared.state().as_ref(),
        &mut exact,
    )
    .unwrap();
    assert_eq!(exact.usage(), usage);

    let mut byte_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: usage.bytes - 1,
        ..limits
    })
    .unwrap();
    assert!(matches!(
        PreparedPublicationSet::seal(
            observe_prepared_destinations(&prepared),
            prepared.state().as_ref(),
            &mut byte_short,
        ),
        Err(PreparedPublicationError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }))
    ));

    let mut entry_short = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: usage.entries - 1,
        ..limits
    })
    .unwrap();
    assert!(matches!(
        PreparedPublicationSet::seal(
            observe_prepared_destinations(&prepared),
            prepared.state().as_ref(),
            &mut entry_short,
        ),
        Err(PreparedPublicationError::Budget(BudgetError::Exceeded {
            resource: "entries",
            ..
        }))
    ));

    let mut member_short = AssetLoadBudget::new(AssetLoadLimits {
        max_members: usage.members - 1,
        ..limits
    })
    .unwrap();
    assert!(matches!(
        PreparedPublicationSet::seal(
            observe_prepared_destinations(&prepared),
            prepared.state().as_ref(),
            &mut member_short,
        ),
        Err(PreparedPublicationError::Budget(BudgetError::Exceeded {
            resource: "members",
            ..
        }))
    ));
    assert_eq!(member_short.usage(), Default::default());
}

#[test]
fn prepared_publication_set_rejects_swapped_source_authority() {
    let (_directory, _workspace, prepared) = prepared_resource_change();
    let publications = prepared.publications().iter().collect::<Vec<_>>();
    assert_eq!(publications.len(), 2);
    let destinations = publications
        .iter()
        .enumerate()
        .map(|(ordinal, publication)| {
            let output = prepared.artifacts().output(publication.output()).unwrap();
            PublicationDestination::exact(
                publications[1 - ordinal].source(),
                publication.output(),
                output.name(),
                publication.target(),
                DestinationExpectation::Observe,
            )
        })
        .collect::<Vec<_>>();
    let proofs = DestinationProofSet::observe(
        prepared.artifacts(),
        &destinations,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();

    assert!(matches!(
        PreparedPublicationSet::seal(
            proofs,
            prepared.state().as_ref(),
            &mut AssetLoadBudget::default(),
        ),
        Err(PreparedPublicationError::SourceBindingMismatch { output: 0, .. })
    ));
}

#[test]
fn prepared_logical_changes_own_canonicalization_and_changed_source_projection() {
    let workspace = WorkspaceId::from_u128(0x1c0).unwrap();
    let first = SourceId::new(workspace, SourceKind::Yaml, 1).unwrap();
    let second = SourceId::new(workspace, SourceKind::Yaml, 2).unwrap();
    let first_object = ObjectId::yaml_document(first, 1).unwrap();
    let second_object = ObjectId::yaml_document(second, 0).unwrap();

    let changes = PreparedLogicalChanges::from_actual_sources_and_touched_objects(
        vec![second, first, first],
        vec![second_object.clone(), first_object.clone(), first_object],
    );
    assert_eq!(changes.sources(), &[first, second]);
    assert_eq!(
        changes.objects(),
        &[ObjectId::yaml_document(first, 1).unwrap(), second_object]
    );

    let outside = ObjectId::yaml_document(second, 1).unwrap();
    let projected =
        PreparedLogicalChanges::from_actual_sources_and_touched_objects(vec![first], vec![outside]);
    assert!(projected.objects().is_empty());
}

#[test]
fn standalone_resource_replace_publishes_a_companion_and_reparses_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(RESOURCE_ALIAS);
    fs::write(&path, RESOURCE_YAML).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    let parent = workspace
        .load_source(
            SourceOpenRequest::new(&path, SourceAlias::new(RESOURCE_ALIAS).unwrap())
                .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let locator = SourceLocator::path(RESOURCE_ALIAS).unwrap();
    let payload = b"standalone replacement payload";

    let prepared = workspace
        .prepare(
            resource_plan(
                &workspace,
                locator.clone(),
                RESOURCE_YAML.as_bytes(),
                payload,
            ),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let sidecar_name = assert_prepared_resource(&prepared, locator, payload, parent, true);
    assert_eq!(prepared.report().artifacts().outputs(), 2);
    assert_eq!(prepared.publications().len(), 2);
    assert!(
        prepared
            .publications()
            .iter()
            .map(|publication| {
                prepared
                    .artifacts()
                    .output(publication.output())
                    .unwrap()
                    .name()
            })
            .is_sorted(),
        "prepared publication authority must retain one deterministic logical-name order"
    );
    for publication in prepared.publications().iter() {
        let artifact = prepared.artifacts().output(publication.output()).unwrap();
        let source = prepared
            .report()
            .sources()
            .iter()
            .find(|report| report.source_id() == publication.source())
            .expect("publication authority source must have a prepared report");
        assert!(source.publication_root());
        assert_eq!(source.artifact_digest(), artifact.artifact().digest());
    }
    let sidecar_output = prepared
        .artifacts()
        .outputs()
        .find(|output| output.name().as_str() == sidecar_name)
        .expect("companion sidecar must own a declared output");
    let mut sidecar_bytes = Vec::new();
    sidecar_output
        .artifact()
        .stream_verified_to(&mut sidecar_bytes)
        .unwrap();
    assert_eq!(sidecar_bytes, payload);
}

#[test]
fn commit_rejects_a_companion_that_appears_after_prepare() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join(RESOURCE_ALIAS);
    fs::write(&source_path, RESOURCE_YAML).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    let parent = workspace
        .load_source(
            SourceOpenRequest::new(&source_path, SourceAlias::new(RESOURCE_ALIAS).unwrap())
                .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let locator = SourceLocator::path(RESOURCE_ALIAS).unwrap();
    let payload = b"standalone replacement payload";
    let prepared = workspace
        .prepare(
            resource_plan(
                &workspace,
                locator.clone(),
                RESOURCE_YAML.as_bytes(),
                payload,
            ),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let base_revision = prepared.report().base_revision();
    let source_before = fs::read(&source_path).unwrap();
    let sidecar_name = assert_prepared_resource(&prepared, locator, payload, parent, true);
    let output = prepared
        .publications()
        .iter()
        .position(|publication| {
            prepared
                .artifacts()
                .output(publication.output())
                .is_ok_and(|output| output.name().as_str() == sidecar_name)
        })
        .expect("companion must own a canonical publication ordinal");
    let sidecar_path = directory.path().join(&sidecar_name);
    let external = b"external companion";
    fs::write(&sidecar_path, external).unwrap();

    let error = workspace
        .commit(
            prepared,
            PublicationTarget::in_place(directory.path()).unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        CommitError::DestinationConflict {
            output: actual,
            ..
        } if actual == output
    ));
    assert_eq!(workspace.revision(), base_revision);
    assert_eq!(fs::read(source_path).unwrap(), source_before);
    assert_eq!(fs::read(sidecar_path).unwrap(), external);
}

#[test]
fn webfile_resource_replace_appends_the_sidecar_to_the_rebuilt_root() {
    const ALIAS: &str = "resource.web";
    const MEMBER: &str = "audio.asset";
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(ALIAS);
    let original = fixture_webfile(MEMBER, RESOURCE_YAML.as_bytes());
    fs::write(&path, &original).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    let root = workspace
        .load_source(
            SourceOpenRequest::new(&path, SourceAlias::new(ALIAS).unwrap())
                .with_kind_hint(SourceKind::WebFile),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let locator = container_locator(ALIAS, ContainmentKind::WebFile, MEMBER);
    let payload = b"webfile replacement payload";

    let prepared = workspace
        .prepare(
            resource_plan(
                &workspace,
                locator.clone(),
                RESOURCE_YAML.as_bytes(),
                payload,
            ),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let sidecar_name = assert_prepared_resource(&prepared, locator, payload, root, false);
    let rebuilt = WebFile::from_bytes(artifact_output_bytes(&prepared)).unwrap();
    assert_eq!(rebuilt.files().len(), 2);
    let sidecar = rebuilt
        .files()
        .iter()
        .find(|file| file.name == sidecar_name)
        .expect("rebuilt WebFile must contain the generated sidecar");
    assert_eq!(
        rebuilt.extract_file_slice_by_info(sidecar).unwrap(),
        payload
    );
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn bundle_resource_replace_appends_the_sidecar_to_the_rebuilt_root() {
    const ALIAS: &str = "resource.bundle";
    const MEMBER: &str = "audio.asset";
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(ALIAS);
    let original = fixture_bundle(MEMBER, RESOURCE_YAML.as_bytes());
    fs::write(&path, &original).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    let root = workspace
        .load_source(
            SourceOpenRequest::new(&path, SourceAlias::new(ALIAS).unwrap())
                .with_kind_hint(SourceKind::AssetBundle),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let locator = container_locator(ALIAS, ContainmentKind::Bundle, MEMBER);
    let payload = b"bundle replacement payload";

    let prepared = workspace
        .prepare(
            resource_plan(
                &workspace,
                locator.clone(),
                RESOURCE_YAML.as_bytes(),
                payload,
            ),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let sidecar_name = assert_prepared_resource(&prepared, locator, payload, root, false);
    let rebuilt = BundleParser::from_bytes(artifact_output_bytes(&prepared)).unwrap();
    assert_eq!(rebuilt.nodes.len(), 2);
    let sidecar = rebuilt
        .nodes
        .iter()
        .find(|node| node.name == sidecar_name)
        .expect("rebuilt bundle must contain the generated sidecar");
    assert_eq!(sidecar.flags, 0);
    assert_eq!(rebuilt.extract_node_data(sidecar).unwrap(), payload);
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn nested_webfile_bundle_resource_replace_places_sidecar_in_direct_bundle_parent() {
    const ALIAS: &str = "resource.web";
    const BUNDLE_MEMBER: &str = "inner.bundle";
    const RESOURCE_MEMBER: &str = "audio.asset";
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(ALIAS);
    let inner_bundle = fixture_bundle(RESOURCE_MEMBER, RESOURCE_YAML.as_bytes());
    let original = fixture_webfile(BUNDLE_MEMBER, &inner_bundle);
    fs::write(&path, &original).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    let root = workspace
        .load_source(
            SourceOpenRequest::new(&path, SourceAlias::new(ALIAS).unwrap())
                .with_kind_hint(SourceKind::WebFile),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let bundle_locator = SourceLocator::path(ALIAS)
        .unwrap()
        .child(
            ContainmentKind::WebFile,
            SourceMemberId::new(BUNDLE_MEMBER).unwrap(),
        )
        .unwrap();
    let locator = bundle_locator
        .clone()
        .child(
            ContainmentKind::Bundle,
            SourceMemberId::new(RESOURCE_MEMBER).unwrap(),
        )
        .unwrap();
    let payload = b"nested bundle replacement payload";

    let prepared = workspace
        .prepare(
            resource_plan(
                &workspace,
                locator.clone(),
                RESOURCE_YAML.as_bytes(),
                payload,
            ),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let sidecar_name = assert_prepared_resource(&prepared, locator, payload, root, false);
    let sidecar = prepared
        .report()
        .sources()
        .iter()
        .find(|source| source.source_id().kind() == SourceKind::StreamedResource)
        .unwrap();
    let expected_sidecar_locator = bundle_locator
        .clone()
        .child(
            ContainmentKind::Bundle,
            SourceMemberId::new(sidecar_name.clone()).unwrap(),
        )
        .unwrap();
    assert_eq!(sidecar.locator(), &expected_sidecar_locator);
    assert_eq!(sidecar.physical_domain_owner(), root);
    let WorkspaceLookup::Resolved(bundle_source) = prepared
        .view()
        .resolve_source(&bundle_locator, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("prepared view must resolve the rebuilt inner bundle");
    };
    let WorkspaceLookup::Resolved(sidecar_source) = prepared
        .view()
        .resolve_source(&expected_sidecar_locator, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("prepared view must resolve the nested sidecar");
    };
    assert_eq!(bundle_source.parent(), Some(root));
    assert_eq!(sidecar_source.parent(), Some(bundle_source.id()));
    assert_eq!(sidecar_source.id(), sidecar.source_id());

    let rebuilt_webfile = WebFile::from_bytes(artifact_output_bytes(&prepared)).unwrap();
    assert_eq!(rebuilt_webfile.files().len(), 1);
    assert!(
        rebuilt_webfile
            .files()
            .iter()
            .all(|file| file.name != sidecar_name)
    );
    let rebuilt_bundle_entry = rebuilt_webfile
        .files()
        .iter()
        .find(|file| file.name == BUNDLE_MEMBER)
        .expect("rebuilt WebFile must retain the inner bundle");
    let rebuilt_bundle = BundleParser::from_bytes(
        rebuilt_webfile
            .extract_file_slice_by_info(rebuilt_bundle_entry)
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert_eq!(rebuilt_bundle.nodes.len(), 2);
    let sidecar_node = rebuilt_bundle
        .nodes
        .iter()
        .find(|node| node.name == sidecar_name)
        .expect("rebuilt inner bundle must contain the generated sidecar");
    assert_eq!(sidecar_node.flags, 0);
    assert_eq!(
        rebuilt_bundle.extract_node_data(sidecar_node).unwrap(),
        payload
    );
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn archive_resource_replace_is_rejected_before_any_zip_publication() {
    const ALIAS: &str = "resource.zip";
    const MEMBER: &str = "audio.asset";
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(ALIAS);
    let original = fixture_archive(MEMBER, RESOURCE_YAML.as_bytes());
    fs::write(&path, &original).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_source(
            SourceOpenRequest::new(&path, SourceAlias::new(ALIAS).unwrap())
                .with_kind_hint(SourceKind::Archive),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let locator = container_locator(ALIAS, ContainmentKind::Archive, MEMBER);

    let error = workspace
        .prepare(
            resource_plan(
                &workspace,
                locator,
                RESOURCE_YAML.as_bytes(),
                b"archive replacement payload",
            ),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    let diagnostic = &error.report().diagnostics()[0];
    assert_eq!(diagnostic.stage(), super::PrepareStage::ArtifactEncoding);
    assert_eq!(
        diagnostic.diagnostic().code(),
        "PREPARE_ARTIFACT_GRAPH_REJECTED"
    );
    assert!(
        diagnostic
            .diagnostic()
            .message()
            .contains("ZIP writing is unsupported")
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn source_changed_after_validation_returns_expected_and_actual_fingerprints() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(TARGET_ALIAS);
    fs::write(&path, TARGET_YAML).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_source(
            SourceOpenRequest::new(&path, SourceAlias::new(TARGET_ALIAS).unwrap())
                .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let changed = TARGET_YAML.replace("m_Name: Before", "m_Name: Change");
    let expected = SourceFingerprint::from_bytes(SourceKind::Yaml, TARGET_YAML.as_bytes());
    let actual = SourceFingerprint::from_bytes(SourceKind::Yaml, changed.as_bytes());
    let mut wrote = false;
    let mut observer = |checkpoint| {
        if checkpoint == PrepareCheckpoint::SourceValidationComplete && !wrote {
            fs::write(&path, &changed).unwrap();
            wrote = true;
        }
    };

    let error = super::runner::prepare_with_test_observer(
        &workspace,
        plan(&workspace, vec![replacement("Before", "After")]),
        PrepareOptions::default(),
        &mut AssetLoadBudget::default(),
        &mut observer,
    )
    .unwrap_err();

    let diagnostic = &error.report().diagnostics()[0];
    assert_eq!(diagnostic.stage(), super::PrepareStage::SourceValidation);
    assert_eq!(diagnostic.expected_fingerprint(), Some(expected));
    assert_eq!(diagnostic.actual_fingerprint(), Some(actual));
    assert_eq!(fs::read(&path).unwrap(), changed.as_bytes());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn non_output_graph_dependency_changed_during_prepare_is_rejected() {
    const DEPENDENCY_ALIAS: &str = "dependency.prefab";
    const DEPENDENCY_YAML: &str = "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &7\nGameObject:\n  m_Name: Dependency\n";
    let directory = tempfile::tempdir().unwrap();
    let owner_path = directory.path().join(TARGET_ALIAS);
    let dependency_path = directory.path().join(DEPENDENCY_ALIAS);
    fs::write(&owner_path, TARGET_YAML).unwrap();
    fs::write(&dependency_path, DEPENDENCY_YAML).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_source(
            SourceOpenRequest::new(&owner_path, SourceAlias::new(TARGET_ALIAS).unwrap())
                .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    workspace
        .load_source(
            SourceOpenRequest::new(
                &dependency_path,
                SourceAlias::new(DEPENDENCY_ALIAS).unwrap(),
            )
            .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let changed = DEPENDENCY_YAML.replace("Dependency", "DependencyChanged");
    let expected = SourceFingerprint::from_bytes(SourceKind::Yaml, DEPENDENCY_YAML.as_bytes());
    let actual = SourceFingerprint::from_bytes(SourceKind::Yaml, changed.as_bytes());
    let mut wrote = false;
    let mut observer = |checkpoint| {
        if checkpoint == PrepareCheckpoint::SourceValidationComplete && !wrote {
            fs::write(&dependency_path, &changed).unwrap();
            wrote = true;
        }
    };

    let error = super::runner::prepare_with_test_observer(
        &workspace,
        plan(&workspace, vec![replacement("Before", "After")]),
        PrepareOptions::default(),
        &mut AssetLoadBudget::default(),
        &mut observer,
    )
    .unwrap_err();

    let diagnostic = &error.report().diagnostics()[0];
    assert_eq!(diagnostic.stage(), super::PrepareStage::SourceValidation);
    assert_eq!(
        diagnostic.source(),
        Some(&SourceLocator::path(DEPENDENCY_ALIAS).unwrap())
    );
    assert_eq!(diagnostic.expected_fingerprint(), Some(expected));
    assert_eq!(diagnostic.actual_fingerprint(), Some(actual));
    assert_eq!(fs::read(&dependency_path).unwrap(), changed.as_bytes());
    assert_eq!(fs::read(&owner_path).unwrap(), TARGET_YAML.as_bytes());
}

#[test]
fn destination_changed_after_observation_returns_expected_and_actual_fingerprints() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(TARGET_ALIAS);
    fs::write(&path, TARGET_YAML).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_source(
            SourceOpenRequest::new(&path, SourceAlias::new(TARGET_ALIAS).unwrap())
                .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let changed = TARGET_YAML.replace("m_Name: Before", "m_Name: Change");
    let expected = SourceFingerprint::from_bytes(SourceKind::Yaml, TARGET_YAML.as_bytes());
    let actual = SourceFingerprint::from_bytes(SourceKind::Yaml, changed.as_bytes());
    let mut wrote = false;
    let mut observer = |checkpoint| {
        if checkpoint == PrepareCheckpoint::DestinationObservationComplete && !wrote {
            fs::write(&path, &changed).unwrap();
            wrote = true;
        }
    };

    let error = super::runner::prepare_with_test_observer(
        &workspace,
        plan(&workspace, vec![replacement("Before", "After")]),
        PrepareOptions::default(),
        &mut AssetLoadBudget::default(),
        &mut observer,
    )
    .unwrap_err();

    let diagnostic = &error.report().diagnostics()[0];
    assert_eq!(
        diagnostic.stage(),
        super::PrepareStage::DestinationValidation
    );
    assert_eq!(diagnostic.expected_fingerprint(), Some(expected));
    assert_eq!(diagnostic.actual_fingerprint(), Some(actual));
    assert_eq!(fs::read(&path).unwrap(), changed.as_bytes());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn mixed_output_destination_conflict_reports_the_matching_source() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(RESOURCE_ALIAS);
    fs::write(&path, RESOURCE_YAML).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_source(
            SourceOpenRequest::new(&path, SourceAlias::new(RESOURCE_ALIAS).unwrap())
                .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let changed = RESOURCE_YAML.replace("offset: 91", "offset: 92");
    let expected = SourceFingerprint::from_bytes(SourceKind::Yaml, RESOURCE_YAML.as_bytes());
    let actual = SourceFingerprint::from_bytes(SourceKind::Yaml, changed.as_bytes());
    let mut wrote = false;
    let mut observer = |checkpoint| {
        if checkpoint == PrepareCheckpoint::DestinationObservationComplete && !wrote {
            fs::write(&path, &changed).unwrap();
            wrote = true;
        }
    };

    let error = super::runner::prepare_with_test_observer(
        &workspace,
        resource_plan(
            &workspace,
            SourceLocator::path(RESOURCE_ALIAS).unwrap(),
            RESOURCE_YAML.as_bytes(),
            b"replacement payload",
        ),
        PrepareOptions::default(),
        &mut AssetLoadBudget::default(),
        &mut observer,
    )
    .unwrap_err();

    let diagnostic = &error.report().diagnostics()[0];
    assert_eq!(
        diagnostic.stage(),
        super::PrepareStage::DestinationValidation
    );
    assert_eq!(
        diagnostic.source(),
        Some(&SourceLocator::path(RESOURCE_ALIAS).unwrap())
    );
    assert_eq!(diagnostic.expected_fingerprint(), Some(expected));
    assert_eq!(diagnostic.actual_fingerprint(), Some(actual));
    assert_eq!(fs::read(&path).unwrap(), changed.as_bytes());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn source_insertion_permutations_preserve_prepare_artifacts_graph_and_reports() {
    let directory = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for index in 0..8 {
        let alias = if index == 3 {
            TARGET_ALIAS.to_owned()
        } else {
            format!("unrelated-{index}.prefab")
        };
        let path = directory.path().join(&alias);
        let bytes = if index == 3 {
            TARGET_YAML.to_owned()
        } else {
            let file_id = index + 1;
            format!(
                "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &{file_id}\nGameObject:\n  m_Name: Unrelated{index}\n"
            )
        };
        fs::write(&path, bytes).unwrap();
        paths.push((alias, path));
    }

    let workspace_id = WorkspaceId::from_u128(0xd3_73_13).unwrap();
    let canonical_order = (0..paths.len()).collect::<Vec<_>>();
    let canonical = workspace_with_order(workspace_id, &paths, &canonical_order);
    let canonical_success = canonical
        .prepare(
            plan(&canonical, vec![replacement("Before", "After")]),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let canonical_outcome = outcome(&canonical_success);
    let canonical_failure = canonical
        .prepare(
            plan(
                &canonical,
                vec![
                    replacement("Before", "Middle"),
                    replacement("Before", "After"),
                ],
            ),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err()
        .into_report();

    for seed in 0..16 {
        let mut order = canonical_order.clone();
        order.shuffle(&mut StdRng::seed_from_u64(seed));
        let workspace = workspace_with_order(workspace_id, &paths, &order);
        assert_eq!(workspace.revision(), canonical.revision());

        let success = workspace
            .prepare(
                plan(&workspace, vec![replacement("Before", "After")]),
                PrepareOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(outcome(&success), canonical_outcome, "seed {seed}");

        let failure: PrepareFailureReport = workspace
            .prepare(
                plan(
                    &workspace,
                    vec![
                        replacement("Before", "Middle"),
                        replacement("Before", "After"),
                    ],
                ),
                PrepareOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err()
            .into_report();
        assert_eq!(failure, canonical_failure, "seed {seed}");
    }
}
