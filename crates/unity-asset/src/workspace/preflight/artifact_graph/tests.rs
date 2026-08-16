use std::fs;
use std::io::{Cursor, Write};
use std::sync::Arc;

use tempfile::TempDir;
use unity_asset_binary::bundle::{AssetBundle, BundleHeader, BundleParser, DirectoryNode};
use unity_asset_binary::compression::CompressionBlock;
use unity_asset_binary::webfile::{WebFile, WebFileCompression};
use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, AssetLoadUsage, ContainmentKind, DigestV1, SourceAlias,
    SourceId, SourceKind, SourceLocator, SourceMemberId, VerifiedSourceImage, WorkspaceId,
};
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactLimits, ArtifactPayload,
    ArtifactSourceDependency, LogicalArtifactName,
};
use zip::CompressionMethod;
use zip::write::FileOptions;

use super::*;
use crate::workspace::source_catalog::{LocatorResolution, SourceCatalog, SourceDescriptor};
use crate::workspace::{AssetWorkspace, SourceOpenRequest};

const YAML_A: &str =
    "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: A\n";
const YAML_B: &str =
    "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: B\n";
const YAML_C: &str =
    "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: C\n";

#[derive(Debug)]
struct GraphOutcome {
    binding_sources: Vec<SourceId>,
    root_sources: Vec<SourceId>,
    root_digests: Vec<DigestV1>,
    root_bytes: Vec<Vec<u8>>,
    source_dependencies: Vec<ArtifactSourceDependency>,
    referenced_source_bytes: u64,
}

fn yaml_source(workspace: WorkspaceId, local: u128, bytes: &[u8]) -> ArtifactPayload {
    let source = SourceId::new(workspace, SourceKind::Yaml, local).unwrap();
    let image = VerifiedSourceImage::verify(SourceKind::Yaml, Arc::<[u8]>::from(bytes));
    ArtifactPayload::source_backed(source, image).unwrap()
}

#[derive(Debug, Clone, Copy)]
enum FixtureBundleEntry<'entry> {
    File {
        name: &'entry str,
        bytes: &'entry [u8],
        flags: u32,
    },
    EmptyDirectory {
        name: &'entry str,
        flags: u32,
    },
    Deleted {
        name: &'entry str,
        flags: u32,
    },
}

fn fixture_bundle(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let entries = entries
        .iter()
        .map(|(name, bytes)| FixtureBundleEntry::File {
            name,
            bytes,
            flags: 0,
        })
        .collect::<Vec<_>>();
    fixture_bundle_entries(&entries)
}

fn fixture_bundle_entries(entries: &[FixtureBundleEntry<'_>]) -> Vec<u8> {
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

    let workspace = WorkspaceId::from_u128(0x711).unwrap();
    let payloads = entries
        .iter()
        .filter_map(|entry| match entry {
            FixtureBundleEntry::File { bytes, .. } => Some(*bytes),
            FixtureBundleEntry::EmptyDirectory { .. } | FixtureBundleEntry::Deleted { .. } => None,
        })
        .enumerate()
        .map(|(index, bytes)| yaml_source(workspace, index as u128 + 1, bytes))
        .collect::<Vec<_>>();
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration
        .declare_output(LogicalArtifactName::new("fixture.bundle").unwrap())
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let handles = payloads
        .iter()
        .map(|payload| batch.prepare_verbatim_source(payload).unwrap())
        .collect::<Vec<_>>();
    let mut handles = handles.into_iter();
    let directory = entries
        .iter()
        .map(|entry| match entry {
            FixtureBundleEntry::File { name, flags, .. } => {
                BundleArtifactEntry::file(&batch, name, *flags, handles.next().unwrap()).unwrap()
            }
            FixtureBundleEntry::EmptyDirectory { name, flags } => {
                BundleArtifactEntry::EmptyDirectory {
                    name,
                    flags: *flags,
                }
            }
            FixtureBundleEntry::Deleted { name, flags } => BundleArtifactEntry::Deleted {
                name,
                flags: *flags,
            },
        })
        .collect::<Vec<_>>();
    assert!(handles.next().is_none());
    let root = BundleWriter::prepare_artifact(
        &mut batch,
        &bundle,
        &directory,
        PackingPolicy::Uncompressed,
    )
    .unwrap();
    batch.bind_output(output, root).unwrap();
    let set = batch.finish().unwrap();
    let mut bytes = Vec::new();
    set.artifact(root)
        .unwrap()
        .stream_verified_to(&mut bytes)
        .unwrap();
    bytes
}

fn fixture_webfile(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let signature = b"UnityWebData1.0\0";
    let directory_bytes = entries.iter().try_fold(0_usize, |total, (name, _)| {
        total.checked_add(12)?.checked_add(name.len())
    });
    let head_length = signature
        .len()
        .checked_add(4)
        .and_then(|length| length.checked_add(directory_bytes.unwrap()))
        .unwrap();
    let mut cursor = head_length;
    let mut bytes = signature.to_vec();
    bytes.extend_from_slice(&i32::try_from(head_length).unwrap().to_le_bytes());
    for (name, payload) in entries {
        bytes.extend_from_slice(&i32::try_from(cursor).unwrap().to_le_bytes());
        bytes.extend_from_slice(&i32::try_from(payload.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(&i32::try_from(name.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        cursor += payload.len();
    }
    for (_, payload) in entries {
        bytes.extend_from_slice(payload);
    }
    bytes
}

fn fixture_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = FileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, payload) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(payload).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn load_snapshot(
    bytes: &[u8],
    alias: &str,
    kind: SourceKind,
) -> (TempDir, WorkspaceSnapshot, SourceId) {
    load_snapshot_with_unrelated(bytes, alias, kind, 0)
}

fn load_snapshot_with_unrelated(
    bytes: &[u8],
    alias: &str,
    kind: SourceKind,
    unrelated_count: usize,
) -> (TempDir, WorkspaceSnapshot, SourceId) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(alias);
    fs::write(&path, bytes).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    let root = workspace
        .load_source(
            SourceOpenRequest::new(path, SourceAlias::new(alias).unwrap()).with_kind_hint(kind),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    for ordinal in 0..unrelated_count {
        let alias = format!("unrelated-{ordinal}.prefab");
        let path = directory.path().join(&alias);
        fs::write(&path, YAML_A).unwrap();
        workspace
            .load_source(
                SourceOpenRequest::new(path, SourceAlias::new(alias).unwrap())
                    .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
    }
    (directory, workspace.snapshot(), root)
}

fn source_at(
    snapshot: &WorkspaceSnapshot,
    alias: &str,
    members: &[(ContainmentKind, &str, u32)],
) -> SourceId {
    let mut locator = SourceLocator::path(alias).unwrap();
    for (containment, name, occurrence) in members {
        locator = locator
            .child(
                *containment,
                SourceMemberId::with_occurrence(*name, *occurrence).unwrap(),
            )
            .unwrap();
    }
    match snapshot.state().catalog().classify_locator(&locator) {
        LocatorResolution::Resolved(source) => source,
        resolution => panic!("fixture locator did not resolve: {resolution:?}"),
    }
}

fn try_run_yaml_graph(
    snapshot: &WorkspaceSnapshot,
    changes: &[(SourceId, &str)],
    output_count: usize,
    load_limits: AssetLoadLimits,
) -> (Result<GraphOutcome, ArtifactGraphError>, AssetLoadUsage) {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::new(load_limits).unwrap();
    let result =
        (|| {
            let mut declaration =
                ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget)?;
            let mut outputs = Vec::new();
            for ordinal in 0..output_count {
                outputs.push(declaration.declare_output(
                    LogicalArtifactName::new(format!("output-{ordinal}")).unwrap(),
                )?);
            }
            let mut batch = declaration.seal_output_names()?;
            let mut leaves = Vec::new();
            for (source, bytes) in changes {
                let mut writer = batch.yaml_writer()?;
                writer.write_all(bytes.as_bytes()).unwrap();
                leaves.push((*source, batch.prepare_yaml_writer(writer)?));
            }

            let graph =
                prepare_artifact_graph(snapshot, snapshot.state().catalog(), &mut batch, &leaves)?;
            assert_eq!(graph.publication_roots().len(), outputs.len());
            let binding_sources = graph
                .bindings()
                .iter()
                .map(|binding| binding.source())
                .collect();
            let root_sources = graph
                .publication_roots()
                .iter()
                .map(|binding| binding.source())
                .collect::<Vec<_>>();
            let roots = graph
                .publication_roots()
                .iter()
                .map(|binding| binding.artifact())
                .collect::<Vec<_>>();
            for (output, root) in outputs.into_iter().zip(roots.iter().copied()) {
                batch.bind_output(output, root)?;
            }
            let set = batch.finish()?;
            let root_digests = roots
                .iter()
                .map(|root| set.artifact(*root).unwrap().digest())
                .collect();
            let root_bytes = roots
                .iter()
                .map(|root| {
                    let mut bytes = Vec::new();
                    set.artifact(*root)
                        .unwrap()
                        .stream_verified_to(&mut bytes)
                        .unwrap();
                    bytes
                })
                .collect();
            Ok(GraphOutcome {
                binding_sources,
                root_sources,
                root_digests,
                root_bytes,
                source_dependencies: set.source_dependencies().to_vec(),
                referenced_source_bytes: set.footprint().referenced_source_bytes(),
            })
        })();
    let usage = inspection_budget.usage();
    (result, usage)
}

fn run_yaml_graph(
    snapshot: &WorkspaceSnapshot,
    changes: &[(SourceId, &str)],
    output_count: usize,
) -> (GraphOutcome, AssetLoadUsage) {
    let (result, usage) =
        try_run_yaml_graph(snapshot, changes, output_count, AssetLoadLimits::default());
    (result.unwrap(), usage)
}

#[test]
fn standalone_leaf_is_the_only_publication_root() {
    let (_directory, snapshot, source) =
        load_snapshot(YAML_A.as_bytes(), "standalone.prefab", SourceKind::Yaml);

    let (outcome, _) = run_yaml_graph(&snapshot, &[(source, YAML_B)], 1);

    assert_eq!(outcome.binding_sources, vec![source]);
    assert_eq!(outcome.root_sources, vec![source]);
    assert_eq!(outcome.root_bytes, vec![YAML_B.as_bytes()]);
}

#[test]
fn new_candidate_companion_leaf_is_an_independent_publication_root() {
    let (_directory, snapshot, parent) =
        load_snapshot(YAML_A.as_bytes(), "companion.prefab", SourceKind::Yaml);
    let resource_bytes = b"new streamed resource";
    let image = VerifiedSourceImage::verify(
        SourceKind::StreamedResource,
        Arc::<[u8]>::from(resource_bytes.as_slice()),
    );
    let fingerprint = image.fingerprint();
    let mut catalog_budget = AssetLoadBudget::default();
    let mut transaction = snapshot
        .state()
        .catalog()
        .begin_transaction(&mut catalog_budget)
        .unwrap();
    let companion = transaction
        .register_companion(
            parent,
            SourceMemberId::new("companion.resS").unwrap(),
            fingerprint,
            &mut catalog_budget,
        )
        .unwrap();
    let candidate = transaction.commit(&mut catalog_budget).unwrap();
    assert!(snapshot.state().store().get(companion).is_none());

    let payload = ArtifactPayload::source_backed(companion, image).unwrap();
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration
        .declare_output(LogicalArtifactName::new("companion.resS").unwrap())
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let leaf = batch.prepare_verbatim_source(&payload).unwrap();

    let graph =
        prepare_artifact_graph(&snapshot, &candidate, &mut batch, &[(companion, leaf)]).unwrap();

    assert_eq!(
        graph.bindings(),
        &[PreparedSourceArtifact::new(companion, leaf)]
    );
    assert_eq!(
        graph.publication_roots(),
        &[PreparedSourceArtifact::new(companion, leaf)]
    );
    batch.bind_output(output, leaf).unwrap();
    let set = batch.finish().unwrap();
    assert_eq!(set.artifact(leaf).unwrap().digest(), fingerprint.digest());
}

fn run_added_sidecar_graph(
    snapshot: &WorkspaceSnapshot,
    container: SourceId,
    name: &str,
    resource_bytes: &[u8],
) -> (SourceId, Vec<u8>) {
    let image = VerifiedSourceImage::verify(
        SourceKind::StreamedResource,
        Arc::<[u8]>::from(resource_bytes),
    );
    let fingerprint = image.fingerprint();
    let mut catalog_budget = AssetLoadBudget::default();
    let mut transaction = snapshot
        .state()
        .catalog()
        .begin_transaction(&mut catalog_budget)
        .unwrap();
    let sidecar = transaction
        .register(
            SourceDescriptor::sidecar(container, SourceMemberId::new(name).unwrap()).unwrap(),
            fingerprint,
            &mut catalog_budget,
        )
        .unwrap();
    let candidate = transaction.commit(&mut catalog_budget).unwrap();

    let payload = ArtifactPayload::source_backed(sidecar, image).unwrap();
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration
        .declare_output(LogicalArtifactName::new("rewritten-container").unwrap())
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let leaf = batch.prepare_verbatim_source(&payload).unwrap();
    let graph =
        prepare_artifact_graph(snapshot, &candidate, &mut batch, &[(sidecar, leaf)]).unwrap();
    assert_eq!(graph.publication_roots().len(), 1);
    assert_eq!(graph.publication_roots()[0].source(), container);
    assert!(
        graph
            .bindings()
            .iter()
            .any(|binding| binding.source() == sidecar && binding.artifact() == leaf)
    );
    let root = graph.publication_roots()[0].artifact();
    batch.bind_output(output, root).unwrap();
    let set = batch.finish().unwrap();
    let mut bytes = Vec::new();
    set.artifact(root)
        .unwrap()
        .stream_verified_to(&mut bytes)
        .unwrap();
    (sidecar, bytes)
}

#[test]
fn new_candidate_webfile_sidecar_is_appended_to_the_container() {
    let web = fixture_webfile(&[("scene.prefab", YAML_A.as_bytes())]);
    let (_directory, snapshot, root) = load_snapshot(&web, "sidecar.web", SourceKind::WebFile);
    let resource_bytes = b"new webfile streamed resource";

    let (_sidecar, rebuilt_bytes) =
        run_added_sidecar_graph(&snapshot, root, "generated.resS", resource_bytes);

    let rebuilt = WebFile::from_bytes(rebuilt_bytes).unwrap();
    assert_eq!(rebuilt.files().len(), 2);
    assert_eq!(rebuilt.files()[0].name, "scene.prefab");
    assert_eq!(rebuilt.files()[1].name, "generated.resS");
    assert_eq!(
        rebuilt
            .extract_file_slice_by_info(&rebuilt.files()[1])
            .unwrap(),
        resource_bytes
    );
}

#[test]
fn new_candidate_bundle_sidecar_is_appended_with_regular_file_flags() {
    let bundle = fixture_bundle(&[("scene.prefab", YAML_A.as_bytes())]);
    let (_directory, snapshot, root) =
        load_snapshot(&bundle, "sidecar.bundle", SourceKind::AssetBundle);
    let resource_bytes = b"new bundle streamed resource";

    let (_sidecar, rebuilt_bytes) =
        run_added_sidecar_graph(&snapshot, root, "generated.resS", resource_bytes);

    let rebuilt = BundleParser::from_bytes(rebuilt_bytes).unwrap();
    assert_eq!(rebuilt.nodes.len(), 2);
    assert_eq!(rebuilt.nodes[0].name, "scene.prefab");
    assert_eq!(rebuilt.nodes[1].name, "generated.resS");
    assert_eq!(rebuilt.nodes[1].flags, 0);
    assert_eq!(
        rebuilt.extract_node_data(&rebuilt.nodes[1]).unwrap(),
        resource_bytes
    );
}

#[test]
fn nested_bundle_and_webfile_are_prepared_once_leaf_to_root() {
    let bundle = fixture_bundle(&[("scene.prefab", YAML_A.as_bytes())]);
    let web = fixture_webfile(&[("inner.bundle", &bundle)]);
    let (_directory, snapshot, root) = load_snapshot(&web, "nested.web", SourceKind::WebFile);
    let inner = source_at(
        &snapshot,
        "nested.web",
        &[(ContainmentKind::WebFile, "inner.bundle", 0)],
    );
    let leaf = source_at(
        &snapshot,
        "nested.web",
        &[
            (ContainmentKind::WebFile, "inner.bundle", 0),
            (ContainmentKind::Bundle, "scene.prefab", 0),
        ],
    );

    let (outcome, _) = run_yaml_graph(&snapshot, &[(leaf, YAML_C)], 1);

    let mut expected_bindings = vec![root, inner, leaf];
    expected_bindings.sort_unstable();
    assert_eq!(outcome.binding_sources, expected_bindings);
    assert_eq!(outcome.root_sources, vec![root]);
    let rebuilt_web = WebFile::from_bytes(outcome.root_bytes[0].clone()).unwrap();
    assert_eq!(rebuilt_web.compression, WebFileCompression::None);
    let rebuilt_bundle = BundleParser::from_bytes(
        rebuilt_web
            .extract_file_slice_by_info(&rebuilt_web.files()[0])
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert_eq!(rebuilt_bundle.header.signature, "UnityFS");
    assert_eq!(rebuilt_bundle.header.flags & 0x3f, 0);
    assert_eq!(rebuilt_bundle.nodes.len(), 1);
    assert_eq!(
        rebuilt_bundle
            .extract_node_data(&rebuilt_bundle.nodes[0])
            .unwrap(),
        YAML_C.as_bytes()
    );
}

#[test]
fn duplicate_names_use_locator_occurrence_and_share_unchanged_source_bytes() {
    let web = fixture_webfile(&[
        ("same.prefab", YAML_A.as_bytes()),
        ("same.prefab", YAML_B.as_bytes()),
    ]);
    let (_directory, snapshot, root) = load_snapshot(&web, "duplicates.web", SourceKind::WebFile);
    let first = source_at(
        &snapshot,
        "duplicates.web",
        &[(ContainmentKind::WebFile, "same.prefab", 0)],
    );
    let second = source_at(
        &snapshot,
        "duplicates.web",
        &[(ContainmentKind::WebFile, "same.prefab", 1)],
    );

    let (outcome, _) = run_yaml_graph(&snapshot, &[(second, YAML_C)], 1);

    assert_eq!(outcome.root_sources, vec![root]);
    let rebuilt = WebFile::from_bytes(outcome.root_bytes[0].clone()).unwrap();
    assert_eq!(
        rebuilt
            .files()
            .iter()
            .map(|file| file.name.as_str())
            .collect::<Vec<_>>(),
        vec!["same.prefab", "same.prefab"]
    );
    assert_eq!(
        rebuilt
            .extract_file_slice_by_info(&rebuilt.files()[0])
            .unwrap(),
        YAML_A.as_bytes()
    );
    assert_eq!(
        rebuilt
            .extract_file_slice_by_info(&rebuilt.files()[1])
            .unwrap(),
        YAML_C.as_bytes()
    );
    assert!(
        outcome
            .source_dependencies
            .iter()
            .any(|dependency| dependency.source() == first)
    );
    assert!(outcome.referenced_source_bytes >= YAML_A.len() as u64);
}

#[test]
fn bundle_duplicate_names_use_locator_occurrence() {
    let bundle = fixture_bundle(&[
        ("same.prefab", YAML_A.as_bytes()),
        ("same.prefab", YAML_B.as_bytes()),
    ]);
    let (_directory, snapshot, root) =
        load_snapshot(&bundle, "duplicates.bundle", SourceKind::AssetBundle);
    let first = source_at(
        &snapshot,
        "duplicates.bundle",
        &[(ContainmentKind::Bundle, "same.prefab", 0)],
    );
    let second = source_at(
        &snapshot,
        "duplicates.bundle",
        &[(ContainmentKind::Bundle, "same.prefab", 1)],
    );

    let (outcome, _) = run_yaml_graph(&snapshot, &[(second, YAML_C)], 1);

    assert_eq!(outcome.root_sources, vec![root]);
    let rebuilt = BundleParser::from_bytes(outcome.root_bytes[0].clone()).unwrap();
    assert_eq!(
        rebuilt
            .nodes
            .iter()
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>(),
        vec!["same.prefab", "same.prefab"]
    );
    assert_eq!(
        rebuilt.extract_node_data(&rebuilt.nodes[0]).unwrap(),
        YAML_A.as_bytes()
    );
    assert_eq!(
        rebuilt.extract_node_data(&rebuilt.nodes[1]).unwrap(),
        YAML_C.as_bytes()
    );
    assert!(
        outcome
            .source_dependencies
            .iter()
            .any(|dependency| dependency.source() == first)
    );
}

#[test]
fn archive_ancestor_is_rejected_before_any_zip_publication() {
    let archive = fixture_archive(&[("scene.prefab", YAML_A.as_bytes())]);
    let (_directory, snapshot, archive_source) =
        load_snapshot(&archive, "assets.zip", SourceKind::Archive);
    let leaf = source_at(
        &snapshot,
        "assets.zip",
        &[(ContainmentKind::Archive, "scene.prefab", 0)],
    );

    let (result, _) =
        try_run_yaml_graph(&snapshot, &[(leaf, YAML_B)], 1, AssetLoadLimits::default());

    assert!(matches!(
        result,
        Err(ArtifactGraphError::UnsupportedArchiveAncestor {
            archive,
            changed_descendant,
        }) if archive == archive_source && changed_descendant == leaf
    ));
}

#[test]
fn archive_above_a_nested_bundle_is_rejected_before_container_rebuild() {
    let bundle = fixture_bundle(&[("scene.prefab", YAML_A.as_bytes())]);
    let archive = fixture_archive(&[("inner.bundle", &bundle)]);
    let (_directory, snapshot, archive_source) =
        load_snapshot(&archive, "nested.zip", SourceKind::Archive);
    let leaf = source_at(
        &snapshot,
        "nested.zip",
        &[
            (ContainmentKind::Archive, "inner.bundle", 0),
            (ContainmentKind::Bundle, "scene.prefab", 0),
        ],
    );

    let (result, _) =
        try_run_yaml_graph(&snapshot, &[(leaf, YAML_B)], 1, AssetLoadLimits::default());

    assert!(matches!(
        result,
        Err(ArtifactGraphError::UnsupportedArchiveAncestor {
            archive,
            changed_descendant,
        }) if archive == archive_source && changed_descendant == leaf
    ));
}

#[test]
fn archive_error_and_budget_are_independent_of_leaf_input_order() {
    let directory = tempfile::tempdir().unwrap();
    let first_bytes = fixture_archive(&[("first.prefab", YAML_A.as_bytes())]);
    let second_bytes = fixture_archive(&[("second.prefab", YAML_A.as_bytes())]);
    let first_path = directory.path().join("first.zip");
    let second_path = directory.path().join("second.zip");
    fs::write(&first_path, first_bytes).unwrap();
    fs::write(&second_path, second_bytes).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    let first_archive = workspace
        .load_source(
            SourceOpenRequest::new(first_path, SourceAlias::new("first.zip").unwrap())
                .with_kind_hint(SourceKind::Archive),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let second_archive = workspace
        .load_source(
            SourceOpenRequest::new(second_path, SourceAlias::new("second.zip").unwrap())
                .with_kind_hint(SourceKind::Archive),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let snapshot = workspace.snapshot();
    let first_leaf = source_at(
        &snapshot,
        "first.zip",
        &[(ContainmentKind::Archive, "first.prefab", 0)],
    );
    let second_leaf = source_at(
        &snapshot,
        "second.zip",
        &[(ContainmentKind::Archive, "second.prefab", 0)],
    );
    let (expected_leaf, expected_archive) = if first_leaf < second_leaf {
        (first_leaf, first_archive)
    } else {
        (second_leaf, second_archive)
    };

    let (forward, forward_usage) = try_run_yaml_graph(
        &snapshot,
        &[(second_leaf, YAML_B), (first_leaf, YAML_C)],
        1,
        AssetLoadLimits::default(),
    );
    let (reverse, reverse_usage) = try_run_yaml_graph(
        &snapshot,
        &[(first_leaf, YAML_C), (second_leaf, YAML_B)],
        1,
        AssetLoadLimits::default(),
    );

    for result in [forward, reverse] {
        assert!(matches!(
            result,
            Err(ArtifactGraphError::UnsupportedArchiveAncestor {
                archive,
                changed_descendant,
            }) if archive == expected_archive && changed_descendant == expected_leaf
        ));
    }
    assert_eq!(forward_usage, reverse_usage);
}

#[test]
fn graph_output_is_deterministic_when_leaf_input_order_changes() {
    let web = fixture_webfile(&[
        ("same.prefab", YAML_A.as_bytes()),
        ("same.prefab", YAML_B.as_bytes()),
    ]);
    let (_directory, snapshot, _root) =
        load_snapshot(&web, "deterministic.web", SourceKind::WebFile);
    let first = source_at(
        &snapshot,
        "deterministic.web",
        &[(ContainmentKind::WebFile, "same.prefab", 0)],
    );
    let second = source_at(
        &snapshot,
        "deterministic.web",
        &[(ContainmentKind::WebFile, "same.prefab", 1)],
    );

    let (forward, _) = run_yaml_graph(&snapshot, &[(first, YAML_B), (second, YAML_C)], 1);
    let (reverse, _) = run_yaml_graph(&snapshot, &[(second, YAML_C), (first, YAML_B)], 1);

    assert_eq!(forward.binding_sources, reverse.binding_sources);
    assert_eq!(forward.root_sources, reverse.root_sources);
    assert_eq!(forward.root_digests, reverse.root_digests);
    assert_eq!(forward.root_bytes, reverse.root_bytes);
}

#[test]
fn graph_allocation_is_rejected_by_the_same_caller_owned_budget() {
    let (_directory, snapshot, source) =
        load_snapshot(YAML_A.as_bytes(), "budget.prefab", SourceKind::Yaml);
    let (_, measured) = run_yaml_graph(&snapshot, &[(source, YAML_B)], 1);
    assert!(measured.bytes > 0);
    let limits = AssetLoadLimits {
        max_bytes: measured.bytes - 1,
        ..AssetLoadLimits::default()
    };

    let (result, rejected_usage) = try_run_yaml_graph(&snapshot, &[(source, YAML_B)], 1, limits);

    assert!(matches!(
        result,
        Err(ArtifactGraphError::Artifact(error))
            if matches!(*error, ArtifactBuildError::LoadBudget(_))
    ));
    assert!(rejected_usage.bytes < measured.bytes);
}

#[test]
fn catalog_member_index_preflights_and_charges_both_catalog_passes() {
    let bundle = fixture_bundle(&[("scene.prefab", YAML_A.as_bytes())]);
    let web = fixture_webfile(&[("inner.bundle", &bundle)]);
    let (_directory, snapshot, _root) =
        load_snapshot(&web, "member-index.web", SourceKind::WebFile);
    let catalog = snapshot.state().catalog();
    let expected_visits = u64::try_from(catalog.len()).unwrap() * 2;

    let measured = {
        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut inspection_budget = AssetLoadBudget::default();
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let members = collect_catalog_member_index(catalog, &mut batch).unwrap();
        assert_eq!(members.len(), 2);
        drop(members);
        drop(batch);
        inspection_budget.usage()
    };
    assert_eq!(measured.members, expected_visits);

    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_members: expected_visits - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let error = collect_catalog_member_index(catalog, &mut batch).unwrap_err();
    assert!(matches!(
        error,
        ArtifactGraphError::Artifact(error)
            if matches!(*error, ArtifactBuildError::LoadBudget(_))
    ));
    assert!(matches!(
        batch.inspect_with_budget(|_| Ok(())),
        Err(ArtifactBuildError::PoisonedBatch)
    ));
    drop(batch);
    assert_eq!(inspection_budget.usage().members, 0);
}

#[test]
fn unrelated_catalog_sources_add_one_metered_index_visit_per_pass() {
    const UNRELATED: usize = 5;
    let bundle = fixture_bundle(&[("scene.prefab", YAML_A.as_bytes())]);
    let web = fixture_webfile(&[("inner.bundle", &bundle)]);
    let (_base_directory, base, _base_root) =
        load_snapshot_with_unrelated(&web, "nested.web", SourceKind::WebFile, 0);
    let (_expanded_directory, expanded, _expanded_root) =
        load_snapshot_with_unrelated(&web, "nested.web", SourceKind::WebFile, UNRELATED);
    let base_leaf = source_at(
        &base,
        "nested.web",
        &[
            (ContainmentKind::WebFile, "inner.bundle", 0),
            (ContainmentKind::Bundle, "scene.prefab", 0),
        ],
    );
    let expanded_leaf = source_at(
        &expanded,
        "nested.web",
        &[
            (ContainmentKind::WebFile, "inner.bundle", 0),
            (ContainmentKind::Bundle, "scene.prefab", 0),
        ],
    );

    let (_, base_usage) = run_yaml_graph(&base, &[(base_leaf, YAML_B)], 1);
    let (_, expanded_usage) = run_yaml_graph(&expanded, &[(expanded_leaf, YAML_B)], 1);

    assert_eq!(
        expanded_usage.members - base_usage.members,
        u64::try_from(UNRELATED).unwrap() * 2
    );
}

#[test]
fn duplicate_leaf_capabilities_are_rejected() {
    let (_directory, snapshot, source) =
        load_snapshot(YAML_A.as_bytes(), "duplicate-leaf.prefab", SourceKind::Yaml);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let mut first = batch.yaml_writer().unwrap();
    first.write_all(YAML_B.as_bytes()).unwrap();
    let first = batch.prepare_yaml_writer(first).unwrap();
    let mut second = batch.yaml_writer().unwrap();
    second.write_all(YAML_C.as_bytes()).unwrap();
    let second = batch.prepare_yaml_writer(second).unwrap();

    let result = prepare_artifact_graph(
        &snapshot,
        snapshot.state().catalog(),
        &mut batch,
        &[(source, first), (source, second)],
    );

    assert!(matches!(
        result,
        Err(ArtifactGraphError::DuplicateLeaf { source_id }) if source_id == source
    ));
    assert!(matches!(
        batch.inspect_with_budget(|_| Ok(())),
        Err(ArtifactBuildError::PoisonedBatch)
    ));
}

#[test]
fn unknown_leaf_error_and_budget_are_independent_of_input_order() {
    let (_directory, snapshot, known) =
        load_snapshot(YAML_A.as_bytes(), "known.prefab", SourceKind::Yaml);
    let lower = SourceId::new(snapshot.workspace_id(), SourceKind::Yaml, u128::MAX - 1).unwrap();
    let higher = SourceId::new(snapshot.workspace_id(), SourceKind::Yaml, u128::MAX).unwrap();
    assert_ne!(known, lower);
    assert_ne!(known, higher);
    assert!(!snapshot.state().catalog().contains(lower));
    assert!(!snapshot.state().catalog().contains(higher));

    let (forward, forward_usage) = try_run_yaml_graph(
        &snapshot,
        &[(higher, YAML_B), (lower, YAML_C)],
        1,
        AssetLoadLimits::default(),
    );
    let (reverse, reverse_usage) = try_run_yaml_graph(
        &snapshot,
        &[(lower, YAML_C), (higher, YAML_B)],
        1,
        AssetLoadLimits::default(),
    );

    assert!(matches!(
        forward,
        Err(ArtifactGraphError::UnknownLeafSource { source_id }) if source_id == lower
    ));
    assert!(matches!(
        reverse,
        Err(ArtifactGraphError::UnknownLeafSource { source_id }) if source_id == lower
    ));
    assert_eq!(forward_usage, reverse_usage);
}

#[test]
fn wire_reconciliation_distinguishes_orphan_and_missing_members() {
    let workspace = WorkspaceId::from_u128(0x712).unwrap();
    let container = SourceId::new(workspace, SourceKind::WebFile, 1).unwrap();
    let source = SourceId::new(workspace, SourceKind::Yaml, 2).unwrap();
    let identity = SourceMemberId::new("known.prefab").unwrap();
    let members = [CatalogMember {
        container,
        source,
        member: &identity,
        seen: false,
    }];
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    let orphan =
        match_catalog_member(&mut batch, container, 3, "orphan.prefab", 0, &members).unwrap_err();
    assert!(matches!(
        orphan,
        ArtifactGraphError::OrphanWireMember {
            container: actual,
            wire_ordinal: 3,
            occurrence: 0,
            ..
        } if actual == container
    ));
    assert!(matches!(
        ensure_all_catalog_members_seen(container, &members),
        Err(ArtifactGraphError::MissingWireMember {
            container: actual,
            source_id,
        }) if actual == container && source_id == source
    ));
}

#[test]
fn bundle_non_file_directory_order_is_retained() {
    let bundle = fixture_bundle_entries(&[
        FixtureBundleEntry::EmptyDirectory {
            name: "before",
            flags: DirectoryNode::DIRECTORY_FLAG | 0x10,
        },
        FixtureBundleEntry::File {
            name: "scene.prefab",
            bytes: YAML_A.as_bytes(),
            flags: 0x20,
        },
        FixtureBundleEntry::Deleted {
            name: "removed",
            flags: DirectoryNode::DELETED_FLAG | 0x40,
        },
        FixtureBundleEntry::EmptyDirectory {
            name: "after",
            flags: DirectoryNode::DIRECTORY_FLAG | 0x80,
        },
    ]);
    let (_directory, snapshot, root) =
        load_snapshot(&bundle, "ordered.bundle", SourceKind::AssetBundle);
    let leaf = source_at(
        &snapshot,
        "ordered.bundle",
        &[(ContainmentKind::Bundle, "scene.prefab", 0)],
    );
    let (outcome, _) = run_yaml_graph(&snapshot, &[(leaf, YAML_B)], 1);
    assert_eq!(outcome.root_sources, vec![root]);
    let rebuilt = BundleParser::from_bytes(outcome.root_bytes[0].clone()).unwrap();
    assert_eq!(
        rebuilt
            .nodes
            .iter()
            .map(|node| (node.name.as_str(), node.flags))
            .collect::<Vec<_>>(),
        vec![
            ("before", DirectoryNode::DIRECTORY_FLAG | 0x10),
            ("scene.prefab", 0x20),
            ("removed", DirectoryNode::DELETED_FLAG | 0x40),
            ("after", DirectoryNode::DIRECTORY_FLAG | 0x80),
        ]
    );
    assert_eq!(
        rebuilt.extract_node_data(&rebuilt.nodes[1]).unwrap(),
        YAML_B.as_bytes()
    );
}

#[test]
fn graph_vectors_charge_actual_capacity_and_reject_one_short() {
    const CAPACITY: usize = 3;
    let measured = {
        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut inspection_budget = AssetLoadBudget::default();
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let values = budgeted_vec::<u64>(&mut batch, CAPACITY, "test_graph_vector").unwrap();
        let actual_capacity = values.capacity();
        drop(values);
        drop(batch);
        let usage = inspection_budget.usage();
        assert_eq!(usage.entries, CAPACITY as u64);
        assert_eq!(
            usage.bytes,
            unity_asset_core::vec_allocation_bytes::<u64>(actual_capacity).unwrap()
        );
        usage.bytes
    };
    assert!(measured > 0);

    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: measured - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let error = budgeted_vec::<u64>(&mut batch, CAPACITY, "test_graph_vector").unwrap_err();
    assert!(matches!(
        error,
        ArtifactGraphError::Artifact(error)
            if matches!(*error, ArtifactBuildError::LoadBudget(_))
    ));
    drop(batch);
    assert_eq!(inspection_budget.usage().bytes, 0);

    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: measured,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    budgeted_vec::<u64>(&mut batch, CAPACITY, "test_graph_vector").unwrap();
    drop(batch);
    assert_eq!(inspection_budget.usage().bytes, measured);
}

#[test]
fn member_error_names_charge_actual_string_capacity_and_reject_one_short() {
    const NAME: &str = "orphan-member.prefab";
    let measured = {
        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut inspection_budget = AssetLoadBudget::default();
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let owned = clone_member_name(&mut batch, NAME).unwrap();
        let actual_capacity = owned.capacity();
        drop(owned);
        drop(batch);
        let usage = inspection_budget.usage();
        assert_eq!(usage.bytes, actual_capacity as u64);
        usage.bytes
    };
    assert!(measured > 0);

    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: measured - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let error = clone_member_name(&mut batch, NAME).unwrap_err();
    assert!(matches!(
        error,
        ArtifactGraphError::Artifact(error)
            if matches!(*error, ArtifactBuildError::LoadBudget(_))
    ));
    drop(batch);
    assert_eq!(inspection_budget.usage().bytes, 0);

    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: measured,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    clone_member_name(&mut batch, NAME).unwrap();
    drop(batch);
    assert_eq!(inspection_budget.usage().bytes, measured);
}

#[test]
fn foreign_candidate_workspace_is_rejected_even_without_leaves() {
    let (_directory, snapshot, _source) =
        load_snapshot(YAML_A.as_bytes(), "workspace.prefab", SourceKind::Yaml);
    let foreign_workspace = WorkspaceId::from_u128(0x713).unwrap();
    assert_ne!(foreign_workspace, snapshot.workspace_id());
    let foreign = SourceCatalog::new(foreign_workspace);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    let error = prepare_artifact_graph(&snapshot, &foreign, &mut batch, &[]).unwrap_err();

    assert!(matches!(
        error,
        ArtifactGraphError::CandidateWorkspaceMismatch { expected, actual }
            if expected == snapshot.workspace_id() && actual == foreign_workspace
    ));
    assert!(matches!(
        batch.inspect_with_budget(|_| Ok(())),
        Err(ArtifactBuildError::PoisonedBatch)
    ));
}
