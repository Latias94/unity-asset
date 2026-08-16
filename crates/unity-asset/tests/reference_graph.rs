use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};

use unity_asset::reference::{
    RawReferenceTarget, ReferenceDirection, ReferenceGraph, ReferenceGraphBuildOptions,
    ReferenceGraphError, ReferenceGuid, ReferenceProjectionFormat, ReferenceProjectionOptions,
    ReferenceResolution, ReferenceTraversalLimits, ReferenceTruncationKind,
};
use unity_asset::workspace::{
    AssetWorkspace, SourceOpenRequest, WorkspaceLookup, WorkspaceOptions, WorkspaceSnapshot,
    WorkspaceView,
};
use unity_asset::{
    AssetLoadBudget, AssetLoadLimits, Diagnostic, FieldPath, ObjectAddress, RevisionedObjectHandle,
    SourceAlias, SourceId, SourceLocator, UnityValue,
};
use unity_asset_binary::asset::{FileIdentifier, SerializedFileParser};
use unity_asset_write::object::{
    SerializedFieldGuard, SerializedObjectEncoder, SerializedObjectMutation,
};
use unity_asset_write::serialized_file::{
    ExternalTableAllocator, SerializedFileEdits, SerializedFileWriter,
};
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

#[path = "support/source_replacement.rs"]
mod source_replacement;

const V22_BINARY: &[u8] =
    include_bytes!("../../unity-asset-write/tests/fixtures/serialized_file_wire/v22.assets.bin");

const TRANSFORM_BINARY: &[u8] = include_bytes!(
    "../../unity-asset-write/tests/fixtures/serialized_file_wire/transform_hierarchy_v22.assets.bin"
);

const LOCAL_GRAPH: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_First: {fileID: 2}
  m_Array:
  - {fileID: 2}
  m_Missing: {fileID: 99}
  m_Null: {fileID: 0}
  m_Invalid: {fileID: 2, unexpected: true}
--- !u!1 &2
GameObject:
  m_Back: {fileID: 1}
"#;

const EXTERNAL_GUID: &str = "0123456789abcdef0123456789abcdef";

const INCREMENTAL_GUID_G1: &str = "11111111111111111111111111111111";
const INCREMENTAL_GUID_G2: &str = "22222222222222222222222222222222";
const INCREMENTAL_OWNER_ALIAS: &str = "owner.prefab";
const INCREMENTAL_BINARY_ALIAS: &str = "binary-owner.assets";
const INCREMENTAL_META_ALIAS: &str = "target.assets.meta";
const INCREMENTAL_TARGET_ALIAS: &str = "target.assets";
const REMAPPED_TARGET_ALIAS: &str = "renamed/target.assets";
const REMAPPED_META_ALIAS: &str = "renamed/target.assets.meta";

const CACHE_SOURCE: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Null: {fileID: 0}
"#;

const CACHE_REVISION_CHANGE: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &2
GameObject:
  m_Name: AddedLater
"#;

fn write(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
}

fn zip_with_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = FileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, payload) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(payload).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn handle(workspace: &AssetWorkspace, source: SourceId, anchor: &str) -> RevisionedObjectHandle {
    let file_id = anchor.parse().unwrap();
    workspace
        .snapshot()
        .objects(&mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .find(|handle| {
            handle.object().source() == source && handle.object().yaml_file_id() == Some(file_id)
        })
        .unwrap()
}

#[derive(Debug)]
struct IncrementalParityPaths {
    owner: PathBuf,
    binary_owner: PathBuf,
    target: PathBuf,
    target_meta: PathBuf,
}

#[derive(Debug, Clone, Copy)]
struct IncrementalParityLayout {
    target_alias: &'static str,
    meta_alias: &'static str,
    target_loaded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalReferenceGraph {
    facts: Vec<CanonicalReferenceFact>,
    diagnostics: Vec<Diagnostic>,
    nodes: Vec<CanonicalObjectIdentity>,
    coverage: CanonicalCoverage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalReferenceFact {
    source: CanonicalObjectIdentity,
    field_path: FieldPath,
    raw_target: RawReferenceTarget,
    resolution: CanonicalResolution,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CanonicalObjectIdentity {
    source: SourceLocator,
    local: CanonicalLocalObjectIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CanonicalLocalObjectIdentity {
    BinaryPathId(i64),
    YamlFileId(i64),
    YamlDocumentOrdinal(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CanonicalResolution {
    Null,
    Resolved(CanonicalObjectIdentity),
    Unloaded(Option<SourceLocator>),
    Missing(Option<ObjectAddress>),
    Ambiguous(Vec<ObjectAddress>),
    Invalid(Diagnostic),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalCoverage {
    total_sources: u64,
    scanned_sources: u64,
    total_nodes: u64,
    indexed_nodes: u64,
    fact_count: u64,
    complete: bool,
    truncations: Vec<CanonicalTruncation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CanonicalTruncation {
    kind: ReferenceTruncationKind,
    limit: u64,
    observed: u64,
}

fn canonical_reference_graph(
    snapshot: &WorkspaceSnapshot,
    graph: &ReferenceGraph,
) -> CanonicalReferenceGraph {
    let locators = snapshot
        .sources(&mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .map(|source| (source.id(), source.locator().clone()))
        .collect::<BTreeMap<_, _>>();
    let canonical_object = |handle: &RevisionedObjectHandle| {
        let object = handle.object();
        let source = locators
            .get(&object.source())
            .expect("every graph object source must have a stable locator")
            .clone();
        let local = if let Some(path_id) = object.binary_path_id() {
            CanonicalLocalObjectIdentity::BinaryPathId(path_id)
        } else if let Some(file_id) = object.yaml_file_id() {
            CanonicalLocalObjectIdentity::YamlFileId(file_id.get())
        } else {
            CanonicalLocalObjectIdentity::YamlDocumentOrdinal(
                object
                    .yaml_document_ordinal()
                    .expect("workspace object must have one format-local identity"),
            )
        };
        CanonicalObjectIdentity { source, local }
    };
    let canonical_resolution = |resolution: &ReferenceResolution| match resolution {
        ReferenceResolution::Null => CanonicalResolution::Null,
        ReferenceResolution::Resolved(target) => {
            CanonicalResolution::Resolved(canonical_object(target))
        }
        ReferenceResolution::Unloaded { source } => CanonicalResolution::Unloaded(source.clone()),
        ReferenceResolution::Missing { target } => CanonicalResolution::Missing(target.clone()),
        ReferenceResolution::Ambiguous { candidates } => {
            CanonicalResolution::Ambiguous(candidates.to_vec())
        }
        ReferenceResolution::Invalid { diagnostic } => {
            CanonicalResolution::Invalid(diagnostic.clone())
        }
    };

    let facts = graph
        .facts()
        .iter()
        .map(|fact| CanonicalReferenceFact {
            source: canonical_object(fact.source()),
            field_path: fact.field_path().clone(),
            raw_target: fact.raw_target().clone(),
            resolution: canonical_resolution(fact.resolution()),
            diagnostics: fact.diagnostics().to_vec(),
        })
        .collect();
    let nodes = graph.nodes().iter().map(canonical_object).collect();
    let coverage = graph.coverage();
    CanonicalReferenceGraph {
        facts,
        diagnostics: graph.diagnostics().to_vec(),
        nodes,
        coverage: CanonicalCoverage {
            total_sources: coverage.total_sources(),
            scanned_sources: coverage.scanned_sources(),
            total_nodes: coverage.total_nodes(),
            indexed_nodes: coverage.indexed_nodes(),
            fact_count: coverage.fact_count(),
            complete: coverage.is_complete(),
            truncations: coverage
                .truncations()
                .iter()
                .map(|truncation| CanonicalTruncation {
                    kind: truncation.kind(),
                    limit: truncation.limit(),
                    observed: truncation.observed(),
                })
                .collect(),
        },
    }
}

fn load_incremental_parity_sources(
    workspace: &mut AssetWorkspace,
    paths: &IncrementalParityPaths,
    layout: IncrementalParityLayout,
) {
    load_with_alias(workspace, &paths.owner, INCREMENTAL_OWNER_ALIAS);
    load_with_alias(workspace, &paths.binary_owner, INCREMENTAL_BINARY_ALIAS);
    load_with_alias(workspace, &paths.target_meta, layout.meta_alias);
    if layout.target_loaded {
        load_with_alias(workspace, &paths.target, layout.target_alias);
    }
}

fn load_with_alias(workspace: &mut AssetWorkspace, path: &Path, alias: &str) -> SourceId {
    let canonical_path = fs::canonicalize(path).unwrap();
    let existing = workspace
        .snapshot()
        .sources(&mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .find(|source| {
            source.parent().is_none()
                && (source.locator().root_alias().as_str() == alias
                    || source.physical_origin() == Some(canonical_path.as_path()))
        })
        .map(|source| source.id());
    if let Some(existing) = existing {
        return source_replacement::replace_source_path(workspace, existing, path, alias);
    }

    workspace
        .load_source(
            SourceOpenRequest::new(path, SourceAlias::new(alias).unwrap()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap()
}

fn assert_incremental_matches_fresh(
    workspace: &AssetWorkspace,
    paths: &IncrementalParityPaths,
    layout: IncrementalParityLayout,
    expected_reused_sources: u64,
) -> CanonicalReferenceGraph {
    let incremental_snapshot = workspace.snapshot();
    let revision = incremental_snapshot.revision();
    let incremental_graph = incremental_snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(incremental_graph.revision(), revision);
    assert_eq!(workspace.revision(), revision);
    assert_eq!(
        incremental_graph
            .build_stats()
            .source_occurrence_cache_hits(),
        expected_reused_sources
    );
    let incremental = canonical_reference_graph(&incremental_snapshot, &incremental_graph);

    let mut fresh_workspace = AssetWorkspace::new().unwrap();
    load_incremental_parity_sources(&mut fresh_workspace, paths, layout);
    let fresh_snapshot = fresh_workspace.snapshot();
    let fresh_graph = fresh_snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(fresh_graph.build_stats().source_occurrence_cache_hits(), 0);
    let fresh = canonical_reference_graph(&fresh_snapshot, &fresh_graph);

    assert_eq!(incremental, fresh);
    incremental
}

fn external_transform_fixture(guid: [u8; 16]) -> Vec<u8> {
    let file = SerializedFileParser::from_bytes(TRANSFORM_BINARY.to_vec()).unwrap();
    let mut budget = AssetLoadBudget::default();
    let mut candidate = SerializedObjectEncoder::new(&file, 2)
        .unwrap()
        .begin_semantic(&mut budget)
        .unwrap();
    let father_path = FieldPath::root().push_field("m_Father").unwrap();
    let mut father = candidate.value_at_path(&father_path).unwrap().clone();
    let guard = SerializedFieldGuard::from_observed(
        candidate.schema_digest(),
        &father_path,
        &father,
        &mut budget,
    )
    .unwrap();
    let father_fields = father
        .as_object_mut()
        .expect("Transform fixture must expose m_Father as a PPtr object");
    father_fields.insert("m_FileID".to_owned(), UnityValue::Integer(1));
    father_fields.insert("m_PathID".to_owned(), UnityValue::Integer(1));
    candidate
        .apply(
            SerializedObjectMutation::replace_field(0, father_path, guard, father),
            &mut budget,
        )
        .unwrap();
    let encoded = candidate.finish(&mut budget).unwrap();
    let mut edits = SerializedFileEdits::default();
    edits
        .try_insert_encoded_object(encoded, &mut budget)
        .unwrap();
    let mut allocator = ExternalTableAllocator::new(&file).unwrap();
    allocator
        .intern(
            FileIdentifier {
                temp_empty: String::new(),
                guid,
                type_: 3,
                path: INCREMENTAL_TARGET_ALIAS.to_owned(),
            },
            &mut budget,
        )
        .unwrap();
    let edits = allocator.into_edits(edits).unwrap();
    SerializedFileWriter::save(&file, &edits).unwrap()
}

fn replace_external_guid(bytes: Vec<u8>, guid: [u8; 16]) -> Vec<u8> {
    let mut file = SerializedFileParser::from_bytes(bytes).unwrap();
    assert_eq!(file.externals.len(), 1);
    file.externals[0].guid = guid;
    SerializedFileWriter::save(&file, &SerializedFileEdits::default()).unwrap()
}

fn incremental_owner(guid: &str) -> String {
    format!(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Target: {{fileID: 1, guid: {guid}, type: 3}}\n  m_Null: {{fileID: 0}}\n  m_Missing: {{fileID: 99}}\n"
    )
}

fn incremental_meta(guid: &str) -> String {
    format!("fileFormatVersion: 2\nguid: {guid}\n")
}

fn canonical_fact<'graph>(
    graph: &'graph CanonicalReferenceGraph,
    source_alias: &str,
    field_path: &str,
) -> &'graph CanonicalReferenceFact {
    graph
        .facts
        .iter()
        .find(|fact| {
            fact.source.source.root_alias().as_str() == source_alias
                && fact.field_path.to_string() == field_path
        })
        .unwrap_or_else(|| panic!("missing canonical fact {source_alias}:{field_path}"))
}

fn canonical_binary_fact<'graph>(
    graph: &'graph CanonicalReferenceGraph,
    source_alias: &str,
    path_id: i64,
    field_path: &str,
) -> &'graph CanonicalReferenceFact {
    graph
        .facts
        .iter()
        .find(|fact| {
            fact.source.source.root_alias().as_str() == source_alias
                && fact.source.local == CanonicalLocalObjectIdentity::BinaryPathId(path_id)
                && fact.field_path.to_string() == field_path
        })
        .unwrap_or_else(|| {
            panic!("missing canonical binary fact {source_alias}:{path_id}:{field_path}")
        })
}

#[test]
fn source_occurrence_cache_outlives_graphs_across_workspace_revisions() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source.prefab");
    let revision_path = directory.path().join("revision.prefab");
    write(&source_path, CACHE_SOURCE);
    write(&revision_path, CACHE_REVISION_CHANGE);

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let old_graph = workspace
        .snapshot()
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(old_graph.build_stats().source_occurrence_cache_hits(), 0);
    drop(old_graph);

    workspace
        .load_path(&revision_path, &mut AssetLoadBudget::default())
        .unwrap();
    let new_graph = workspace
        .snapshot()
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(new_graph.build_stats().source_occurrence_cache_hits(), 1);
}

#[test]
fn shared_fingerprint_cache_survives_when_one_source_owner_is_unloaded() {
    let directory = tempfile::tempdir().unwrap();
    let first_path = directory.path().join("first.prefab");
    let second_path = directory.path().join("second.prefab");
    write(&first_path, CACHE_SOURCE);
    write(&second_path, CACHE_SOURCE);

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&first_path, &mut AssetLoadBudget::default())
        .unwrap();
    let second = workspace
        .load_path(&second_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let old_graph = snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(old_graph.build_stats().source_occurrence_cache_hits(), 1);
    drop(old_graph);
    drop(snapshot);

    workspace
        .unload_source(second, &mut AssetLoadBudget::default())
        .unwrap();
    let new_graph = workspace
        .snapshot()
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(new_graph.build_stats().source_occurrence_cache_hits(), 1);
}

#[test]
fn cache_hits_rebind_to_reloaded_content_backing() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source.prefab");
    let revision_path = directory.path().join("revision.prefab");
    write(&source_path, CACHE_SOURCE);
    write(&revision_path, CACHE_REVISION_CHANGE);

    let mut workspace = AssetWorkspace::new().unwrap();
    let source = workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let old_snapshot = workspace.snapshot();
    let old_graph = old_snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    drop(old_graph);

    workspace
        .unload_source(source, &mut AssetLoadBudget::default())
        .unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let reloaded_graph = workspace
        .snapshot()
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(
        reloaded_graph.build_stats().source_occurrence_cache_hits(),
        1
    );
    drop(reloaded_graph);
    drop(old_snapshot);

    workspace
        .load_path(&revision_path, &mut AssetLoadBudget::default())
        .unwrap();
    let next_graph = workspace
        .snapshot()
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(next_graph.build_stats().source_occurrence_cache_hits(), 1);
}

#[test]
fn one_index_preserves_occurrences_and_serves_all_graph_queries() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("local.prefab");
    write(&path, LOCAL_GRAPH);

    let mut workspace = AssetWorkspace::new().unwrap();
    let source = workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let revision = snapshot.revision();
    let graph = snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(snapshot.revision(), revision);
    assert_eq!(workspace.revision(), revision);
    assert!(graph.is_complete());

    let source_object = handle(&workspace, source, "1");
    let target_object = handle(&workspace, source, "2");
    let outgoing = graph.outgoing(&source_object).unwrap().collect::<Vec<_>>();
    assert_eq!(outgoing.len(), 5);
    assert_eq!(
        outgoing
            .iter()
            .filter(|fact| matches!(fact.resolution(), ReferenceResolution::Resolved(target) if target == &target_object))
            .count(),
        2
    );
    assert_eq!(
        outgoing
            .iter()
            .filter(|fact| matches!(fact.resolution(), ReferenceResolution::Missing { .. }))
            .count(),
        1
    );
    assert_eq!(
        outgoing
            .iter()
            .filter(|fact| matches!(fact.resolution(), ReferenceResolution::Null))
            .count(),
        1
    );
    assert_eq!(
        outgoing
            .iter()
            .filter(|fact| matches!(fact.resolution(), ReferenceResolution::Invalid { .. }))
            .count(),
        1
    );

    let target_paths = graph
        .incoming(&target_object)
        .unwrap()
        .map(|fact| fact.field_path().to_string())
        .collect::<Vec<_>>();
    assert_eq!(target_paths, ["$.m_Array[0]", "$.m_First"]);

    let closure = graph
        .closure(
            std::slice::from_ref(&source_object),
            ReferenceDirection::Outgoing,
            ReferenceTraversalLimits::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(closure.is_complete());
    assert_eq!(closure.len(), 2);
    assert_eq!(
        graph
            .cycles(
                ReferenceTraversalLimits::unbounded(),
                &mut AssetLoadBudget::default()
            )
            .unwrap()
            .len(),
        1
    );
    assert_eq!(graph.roots().count(), 0);
    assert_eq!(graph.leaves().count(), 0);

    let limited = snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::default().with_max_facts(2),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(!limited.is_complete());
    assert_eq!(limited.facts().len(), 2);
    assert_eq!(limited.coverage().truncations().len(), 1);

    let mut json = Vec::new();
    let report = graph
        .write_projection(
            &mut json,
            ReferenceProjectionOptions::new(ReferenceProjectionFormat::JsonV2),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(report.is_complete());
    assert_eq!(report.facts_written(), 6);
    let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(
        value["schema"],
        unity_asset::reference::REFERENCE_GRAPH_PROJECTION_SCHEMA
    );
    assert_eq!(value["facts"].as_array().unwrap().len(), 6);

    let mut second = Vec::new();
    graph
        .write_projection(
            &mut second,
            ReferenceProjectionOptions::new(ReferenceProjectionFormat::JsonV2),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(json, second);

    let mut dot = Vec::new();
    graph
        .write_projection(
            &mut dot,
            ReferenceProjectionOptions::new(ReferenceProjectionFormat::DotV2),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let dot = String::from_utf8(dot).unwrap();
    assert!(dot.contains("$.m_Array[0]"));
    assert!(dot.ends_with("}\n"));

    let limits = AssetLoadLimits {
        max_entries: 64,
        max_bytes: u64::try_from(json.len() - 1).unwrap(),
        max_members: 64,
        ..AssetLoadLimits::default()
    };
    let mut one_short = AssetLoadBudget::new(limits).unwrap();
    let error = graph
        .write_projection(
            &mut Vec::new(),
            ReferenceProjectionOptions::new(ReferenceProjectionFormat::JsonV2),
            &mut one_short,
        )
        .unwrap_err();
    assert!(matches!(error, ReferenceGraphError::Budget(_)));
}

#[test]
fn node_soft_limits_never_turn_loaded_targets_into_missing_references() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("local.prefab");
    write(&path, LOCAL_GRAPH);

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let complete = snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(complete.nodes().len(), 2);
    assert_eq!(complete.facts().len(), 6);

    let empty = snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded().with_max_nodes(0),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(empty.nodes().is_empty());
    assert!(empty.facts().is_empty());
    assert_eq!(empty.coverage().total_nodes(), 2);
    assert_eq!(empty.coverage().indexed_nodes(), 0);
    assert_eq!(empty.coverage().truncations().len(), 1);
    assert_eq!(
        empty.coverage().truncations()[0].kind(),
        ReferenceTruncationKind::Nodes
    );

    let prefix = snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded().with_max_nodes(1),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(prefix.nodes().len(), 1);
    assert_eq!(prefix.facts().len(), 3);
    assert_eq!(prefix.coverage().total_nodes(), 2);
    assert_eq!(prefix.coverage().indexed_nodes(), 1);
    assert!(prefix.facts().iter().all(|fact| {
        !matches!(
            fact.field_path().to_string().as_str(),
            "$.m_Array[0]" | "$.m_First"
        )
    }));
    assert_eq!(
        prefix
            .facts()
            .iter()
            .filter(|fact| matches!(fact.resolution(), ReferenceResolution::Missing { .. }))
            .map(|fact| fact.field_path().to_string())
            .collect::<Vec<_>>(),
        ["$.m_Missing"]
    );
    assert_eq!(
        prefix
            .coverage()
            .truncations()
            .iter()
            .map(|truncation| (truncation.kind(), truncation.limit(), truncation.observed()))
            .collect::<Vec<_>>(),
        [
            (ReferenceTruncationKind::Nodes, 1, 2),
            (ReferenceTruncationKind::Facts, 3, 5),
        ]
    );

    let full_limit = snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded().with_max_nodes(2),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(full_limit.is_complete());
    assert_eq!(full_limit.nodes(), complete.nodes());
    assert_eq!(full_limit.facts(), complete.facts());
}

#[test]
fn projections_separate_revision_context_from_portable_object_addresses() {
    let directory = tempfile::tempdir().unwrap();
    let first_path = directory.path().join("first.prefab");
    let second_path = directory.path().join("second.prefab");
    write(&first_path, LOCAL_GRAPH);
    write(&second_path, LOCAL_GRAPH);

    let mut first_workspace = AssetWorkspace::new().unwrap();
    let mut second_workspace = AssetWorkspace::new().unwrap();
    load_with_alias(&mut first_workspace, &first_path, "local.prefab");
    load_with_alias(&mut second_workspace, &second_path, "local.prefab");
    let first_snapshot = first_workspace.snapshot();
    let second_snapshot = second_workspace.snapshot();
    let first_graph = first_snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let second_graph = second_snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let mut first_json = Vec::new();
    let mut second_json = Vec::new();
    first_graph
        .write_projection(
            &mut first_json,
            ReferenceProjectionOptions::new(ReferenceProjectionFormat::JsonV2),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    second_graph
        .write_projection(
            &mut second_json,
            ReferenceProjectionOptions::new(ReferenceProjectionFormat::JsonV2),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut first_value: serde_json::Value = serde_json::from_slice(&first_json).unwrap();
    let mut second_value: serde_json::Value = serde_json::from_slice(&second_json).unwrap();
    assert_ne!(first_value["workspace"], second_value["workspace"]);
    assert_ne!(first_value["revision"], second_value["revision"]);
    for value in [&mut first_value, &mut second_value] {
        value.as_object_mut().unwrap().remove("workspace");
        value.as_object_mut().unwrap().remove("revision");
    }
    assert_eq!(first_value, second_value);

    for node in first_graph.nodes() {
        let address = first_graph.address(node).unwrap();
        let resolved = second_snapshot
            .resolve_object(address, &mut AssetLoadBudget::default())
            .unwrap();
        let WorkspaceLookup::Resolved(handle) = resolved else {
            panic!("portable graph address did not resolve in an equivalent workspace");
        };
        assert_eq!(second_graph.address(&handle).unwrap(), address);
    }
}

#[test]
fn canonical_projection_is_independent_of_graph_and_fact_cache_warmth() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("local.prefab");
    write(&path, LOCAL_GRAPH);

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let cold = snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(!cold.build_stats().graph_cache_hit());
    assert_eq!(cold.build_stats().source_occurrence_cache_hits(), 0);
    let mut cold_json = Vec::new();
    cold.write_projection(
        &mut cold_json,
        ReferenceProjectionOptions::new(ReferenceProjectionFormat::JsonV2),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();

    let graph_cached = snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(graph_cached.build_stats().graph_cache_hit());
    assert_eq!(graph_cached.build_stats().source_occurrence_cache_hits(), 0);
    let mut graph_cached_json = Vec::new();
    graph_cached
        .write_projection(
            &mut graph_cached_json,
            ReferenceProjectionOptions::new(ReferenceProjectionFormat::JsonV2),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(cold_json, graph_cached_json);
    drop(graph_cached);
    drop(cold);

    let warm = snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(!warm.build_stats().graph_cache_hit());
    assert_eq!(warm.build_stats().source_occurrence_cache_hits(), 1);
    let mut warm_json = Vec::new();
    warm.write_projection(
        &mut warm_json,
        ReferenceProjectionOptions::new(ReferenceProjectionFormat::JsonV2),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();

    assert_eq!(cold_json, warm_json);
}

#[test]
fn archive_member_meta_guid_resolves_its_sibling_without_physical_member_paths() {
    let directory = tempfile::tempdir().unwrap();
    let archive_path = directory.path().join("project.zip");
    let owner = format!(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Target: {{fileID: 123, guid: {EXTERNAL_GUID}, type: 3}}\n"
    );
    let target = b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &123\nGameObject:\n  m_Name: Target\n";
    let meta = format!("fileFormatVersion: 2\nguid: {EXTERNAL_GUID}\n");
    fs::write(
        &archive_path,
        zip_with_entries(&[
            ("nested/owner.prefab", owner.as_bytes()),
            ("nested/target.prefab", target),
            ("nested/target.prefab.meta", meta.as_bytes()),
        ]),
    )
    .unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&archive_path, &mut AssetLoadBudget::default())
        .unwrap();
    let graph = workspace
        .snapshot()
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let fact = graph
        .facts()
        .iter()
        .find(|fact| fact.field_path().to_string() == "$.m_Target")
        .expect("archive member reference fact");
    let ReferenceResolution::Resolved(target) = fact.resolution() else {
        panic!("archive member GUID did not resolve its sibling target");
    };
    let target_address = graph.address(target).unwrap();
    assert_eq!(
        target_address
            .source_locator()
            .members()
            .last()
            .unwrap()
            .name(),
        "nested/target.prefab"
    );
}

#[test]
fn duplicate_archive_members_pair_meta_guids_and_project_exact_occurrences() {
    let directory = tempfile::tempdir().unwrap();
    let archive_path = directory.path().join("duplicates.zip");
    let owner = format!(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_First: {{fileID: 123, guid: {INCREMENTAL_GUID_G1}, type: 3}}\n  m_Second: {{fileID: 123, guid: {INCREMENTAL_GUID_G2}, type: 3}}\n"
    );
    let target = b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &123\nGameObject:\n  m_Name: Target\n";
    let first_meta = format!("fileFormatVersion: 2\nguid: {INCREMENTAL_GUID_G1}\n");
    let second_meta = format!("fileFormatVersion: 2\nguid: {INCREMENTAL_GUID_G2}\n");
    fs::write(
        &archive_path,
        zip_with_entries(&[
            ("nested/owner.prefab", owner.as_bytes()),
            ("nested/target.prefab", target),
            ("nested/target.prefab", target),
            ("nested/target.prefab.meta", first_meta.as_bytes()),
            ("nested/target.prefab.meta", second_meta.as_bytes()),
        ]),
    )
    .unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&archive_path, &mut AssetLoadBudget::default())
        .unwrap();
    let graph = workspace
        .snapshot()
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    for (field, expected_occurrence) in [("$.m_First", 0), ("$.m_Second", 1)] {
        let fact = graph
            .facts()
            .iter()
            .find(|fact| fact.field_path().to_string() == field)
            .unwrap_or_else(|| panic!("missing duplicate-member fact {field}"));
        let ReferenceResolution::Resolved(target) = fact.resolution() else {
            panic!("duplicate-member fact {field} did not resolve");
        };
        assert_eq!(
            graph
                .address(target)
                .unwrap()
                .source_locator()
                .members()
                .last()
                .unwrap()
                .member()
                .same_name_occurrence(),
            expected_occurrence
        );
    }

    let mut dot = Vec::new();
    graph
        .write_projection(
            &mut dot,
            ReferenceProjectionOptions::new(ReferenceProjectionFormat::DotV2),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let dot = String::from_utf8(dot).unwrap();
    assert!(dot.contains("::archive[occurrence=0]:nested/target.prefab"));
    assert!(dot.contains("::archive[occurrence=1]:nested/target.prefab"));
}

#[test]
fn physical_sidecar_relocation_preserves_logical_revision_and_handles() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    let owner_path = directory.path().join("owner.prefab");
    let first_target = first.join("target.prefab");
    let first_meta = first.join("target.prefab.meta");
    let second_target = second.join("target.prefab");
    let detached_meta = second.join("detached.prefab.meta");
    write(
        &owner_path,
        &format!(
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Target: {{fileID: 123, guid: {EXTERNAL_GUID}, type: 3}}\n"
        ),
    );
    for path in [&first_target, &second_target] {
        write(
            path,
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &123\nGameObject:\n  m_Name: Target\n",
        );
    }
    for path in [&first_meta, &detached_meta] {
        write(
            path,
            &format!("fileFormatVersion: 2\nguid: {EXTERNAL_GUID}\n"),
        );
    }

    let mut workspace = AssetWorkspace::new().unwrap();
    load_with_alias(&mut workspace, &owner_path, "owner.prefab");
    let target = load_with_alias(&mut workspace, &first_target, "logical/target.prefab");
    let meta = load_with_alias(&mut workspace, &first_meta, "logical/target.prefab.meta");
    let old_snapshot = workspace.snapshot();
    let old_graph = old_snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(old_graph.facts().iter().any(|fact| {
        fact.field_path().to_string() == "$.m_Target"
            && matches!(fact.resolution(), ReferenceResolution::Resolved(_))
    }));
    let revision = old_snapshot.revision();
    let retained_handle = old_graph.nodes()[0].clone();

    workspace
        .unload_source(target, &mut AssetLoadBudget::default())
        .unwrap();
    workspace
        .unload_source(meta, &mut AssetLoadBudget::default())
        .unwrap();
    load_with_alias(&mut workspace, &second_target, "logical/target.prefab");
    load_with_alias(&mut workspace, &detached_meta, "logical/target.prefab.meta");
    let new_snapshot = workspace.snapshot();
    assert_eq!(new_snapshot.revision(), revision);
    let new_graph = new_snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(new_graph.facts().iter().any(|fact| {
        fact.field_path().to_string() == "$.m_Target"
            && matches!(fact.resolution(), ReferenceResolution::Resolved(_))
    }));

    new_graph.outgoing(&retained_handle).unwrap();
    assert_eq!(old_graph.revision(), new_graph.revision());
    assert_eq!(old_snapshot.revision(), new_snapshot.revision());
}

#[test]
fn workspace_typetree_policy_controls_graph_failure_and_completeness() {
    const OBJECT_BYTE_SIZE_OFFSET: usize = 176;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("malformed.assets");
    let mut malformed = V22_BINARY.to_vec();
    malformed[OBJECT_BYTE_SIZE_OFFSET..OBJECT_BYTE_SIZE_OFFSET + 4]
        .copy_from_slice(&2_u32.to_be_bytes());
    fs::write(&path, malformed).unwrap();

    let mut lenient = AssetWorkspace::with_options(WorkspaceOptions::lenient()).unwrap();
    lenient
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let lenient_graph = lenient
        .snapshot()
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(!lenient_graph.is_complete());
    assert!(
        lenient_graph
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code() == "REFERENCE_OBJECT_SCAN_FAILED")
    );

    let mut strict = AssetWorkspace::with_options(WorkspaceOptions::strict()).unwrap();
    strict
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let error = strict
        .snapshot()
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(error, ReferenceGraphError::Binary(_)));
}

#[test]
fn guid_resolution_changes_without_hidden_loading_or_stale_facts() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source.prefab");
    let target_path = directory.path().join("target.asset");
    let target_meta_path = directory.path().join("target.asset.meta");
    let second_target_path = directory.path().join("second.asset");
    let second_meta_path = directory.path().join("second.asset.meta");

    write(
        &source_path,
        &format!(
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Target: {{fileID: 123, guid: {EXTERNAL_GUID}, type: 3}}\n  m_Missing: {{fileID: 999, guid: {EXTERNAL_GUID}, type: 3}}\n"
        ),
    );
    write(
        &target_meta_path,
        &format!("fileFormatVersion: 2\nguid: {EXTERNAL_GUID}\n"),
    );
    write(
        &target_path,
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &123\nGameObject:\n  m_Name: Target\n",
    );
    write(
        &second_meta_path,
        &format!("fileFormatVersion: 2\nguid: {EXTERNAL_GUID}\n"),
    );
    write(
        &second_target_path,
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &123\nGameObject:\n  m_Name: Second\n",
    );

    let mut workspace = AssetWorkspace::new().unwrap();
    let source = workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    workspace
        .load_path(&target_meta_path, &mut AssetLoadBudget::default())
        .unwrap();

    let unloaded_snapshot = workspace.snapshot();
    let unloaded_revision = unloaded_snapshot.revision();
    let unloaded = unloaded_snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(unloaded_snapshot.revision(), unloaded_revision);
    assert_eq!(workspace.revision(), unloaded_revision);
    assert_eq!(
        unloaded
            .facts()
            .iter()
            .filter(|fact| fact.source().object().source() == source)
            .filter(|fact| matches!(fact.resolution(), ReferenceResolution::Unloaded { .. }))
            .count(),
        2
    );
    drop(unloaded);

    let target = workspace
        .load_path(&target_path, &mut AssetLoadBudget::default())
        .unwrap();
    let loaded_snapshot = workspace.snapshot();
    let loaded_revision = loaded_snapshot.revision();
    assert_ne!(loaded_revision, unloaded_revision);
    let loaded = loaded_snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(loaded.build_stats().source_occurrence_cache_hits(), 2);
    assert_eq!(workspace.revision(), loaded_revision);
    let source_facts = loaded
        .facts()
        .iter()
        .filter(|fact| fact.source().object().source() == source)
        .collect::<Vec<_>>();
    assert_eq!(
        source_facts
            .iter()
            .filter(|fact| matches!(fact.resolution(), ReferenceResolution::Resolved(_)))
            .count(),
        1
    );
    assert_eq!(
        source_facts
            .iter()
            .filter(|fact| matches!(fact.resolution(), ReferenceResolution::Missing { .. }))
            .count(),
        1
    );

    workspace
        .load_path(&second_meta_path, &mut AssetLoadBudget::default())
        .unwrap();
    workspace
        .load_path(&second_target_path, &mut AssetLoadBudget::default())
        .unwrap();
    let ambiguous_snapshot = workspace.snapshot();
    let ambiguous = ambiguous_snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(ambiguous.build_stats().source_occurrence_cache_hits() >= 3);
    assert_eq!(workspace.revision(), ambiguous_snapshot.revision());
    assert_eq!(
        ambiguous
            .facts()
            .iter()
            .filter(|fact| fact.source().object().source() == source)
            .filter(|fact| matches!(fact.resolution(), ReferenceResolution::Ambiguous { .. }))
            .count(),
        2
    );

    workspace
        .unload_source(target, &mut AssetLoadBudget::default())
        .unwrap();
    let after_unload = workspace.snapshot();
    assert_ne!(after_unload.revision(), loaded_revision);
    let graph = after_unload
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(workspace.revision(), after_unload.revision());
    assert!(graph.facts().iter().any(|fact| {
        fact.source().object().source() == source
            && matches!(fact.resolution(), ReferenceResolution::Resolved(_))
    }));
}

#[test]
fn binary_and_yaml_adapters_bind_into_the_same_revisioned_fact_model() {
    let directory = tempfile::tempdir().unwrap();
    let binary_path = directory.path().join("transforms.assets");
    let meta_path = directory.path().join("transforms.assets.meta");
    let yaml_path = directory.path().join("bridge.prefab");
    fs::write(&binary_path, TRANSFORM_BINARY).unwrap();
    write(
        &meta_path,
        &format!("fileFormatVersion: 2\nguid: {EXTERNAL_GUID}\n"),
    );
    write(
        &yaml_path,
        &format!(
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Binary: {{fileID: 1, guid: {EXTERNAL_GUID}, type: 3}}\n"
        ),
    );

    let mut workspace = AssetWorkspace::new().unwrap();
    let binary = workspace
        .load_path(&binary_path, &mut AssetLoadBudget::default())
        .unwrap();
    workspace
        .load_path(&meta_path, &mut AssetLoadBudget::default())
        .unwrap();
    let yaml = workspace
        .load_path(&yaml_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let graph = snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert!(graph.facts().iter().any(|fact| {
        fact.source().object().source() == binary
            && matches!(
                fact.raw_target(),
                unity_asset::reference::RawReferenceTarget::Binary { .. }
            )
    }));
    assert!(graph.facts().iter().any(|fact| {
        fact.source().object().source() == yaml
            && matches!(
                fact.resolution(),
                ReferenceResolution::Resolved(target)
                    if target.object().source() == binary
                        && target.object().binary_path_id() == Some(1)
            )
    }));
    assert_eq!(workspace.revision(), graph.revision());
}

#[test]
fn incremental_reference_resolution_matches_a_fresh_full_rebuild_after_identity_changes() {
    let directory = tempfile::tempdir().unwrap();
    let paths = IncrementalParityPaths {
        owner: directory.path().join("owner.prefab"),
        binary_owner: directory.path().join("binary-owner.assets"),
        target: directory.path().join("target.assets"),
        target_meta: directory.path().join("target.assets.meta"),
    };
    let binary_g1 = external_transform_fixture([0x11; 16]);
    let binary_g2 = replace_external_guid(binary_g1.clone(), [0x22; 16]);
    write(&paths.owner, &incremental_owner(INCREMENTAL_GUID_G1));
    fs::write(&paths.binary_owner, &binary_g1).unwrap();
    fs::write(&paths.target, TRANSFORM_BINARY).unwrap();
    write(&paths.target_meta, &incremental_meta(INCREMENTAL_GUID_G1));

    let initial_layout = IncrementalParityLayout {
        target_alias: INCREMENTAL_TARGET_ALIAS,
        meta_alias: INCREMENTAL_META_ALIAS,
        target_loaded: true,
    };
    let mut workspace = AssetWorkspace::new().unwrap();
    load_incremental_parity_sources(&mut workspace, &paths, initial_layout);
    let initial_revision = workspace.revision();
    let initial = assert_incremental_matches_fresh(&workspace, &paths, initial_layout, 0);
    assert!(matches!(
        canonical_fact(&initial, INCREMENTAL_OWNER_ALIAS, "$.m_Target").resolution,
        CanonicalResolution::Resolved(_)
    ));
    let initial_binary = canonical_binary_fact(&initial, INCREMENTAL_BINARY_ALIAS, 2, "$.m_Father");
    assert!(matches!(
        &initial_binary.raw_target,
        RawReferenceTarget::Binary {
            file_id: 1,
            path_id: 1,
            external: Some(external),
        } if external.guid() == Some([0x11; 16])
    ));
    assert!(matches!(
        initial_binary.resolution,
        CanonicalResolution::Resolved(_)
    ));

    write(&paths.owner, &incremental_owner(INCREMENTAL_GUID_G2));
    load_with_alias(&mut workspace, &paths.owner, INCREMENTAL_OWNER_ALIAS);
    assert_ne!(workspace.revision(), initial_revision);
    let source_changed_revision = workspace.revision();
    let source_changed = assert_incremental_matches_fresh(&workspace, &paths, initial_layout, 3);
    assert_ne!(source_changed, initial);
    let owner_target = canonical_fact(&source_changed, INCREMENTAL_OWNER_ALIAS, "$.m_Target");
    assert!(matches!(
        &owner_target.raw_target,
        RawReferenceTarget::Yaml {
            guid: Some(ReferenceGuid::Parsed(guid)),
            ..
        } if *guid == [0x22; 16]
    ));
    assert!(matches!(
        owner_target.resolution,
        CanonicalResolution::Unloaded(_)
    ));

    write(&paths.target_meta, &incremental_meta(INCREMENTAL_GUID_G2));
    load_with_alias(&mut workspace, &paths.target_meta, INCREMENTAL_META_ALIAS);
    assert_ne!(workspace.revision(), source_changed_revision);
    let meta_changed_revision = workspace.revision();
    let meta_changed = assert_incremental_matches_fresh(&workspace, &paths, initial_layout, 3);
    assert!(matches!(
        canonical_fact(&meta_changed, INCREMENTAL_OWNER_ALIAS, "$.m_Target").resolution,
        CanonicalResolution::Resolved(_)
    ));
    let conflicting_binary =
        canonical_binary_fact(&meta_changed, INCREMENTAL_BINARY_ALIAS, 2, "$.m_Father");
    assert!(matches!(
        conflicting_binary.resolution,
        CanonicalResolution::Invalid(ref diagnostic)
            if diagnostic.code() == "REFERENCE_EXTERNAL_IDENTITY_CONFLICT"
    ));

    fs::write(&paths.binary_owner, &binary_g2).unwrap();
    load_with_alias(
        &mut workspace,
        &paths.binary_owner,
        INCREMENTAL_BINARY_ALIAS,
    );
    assert_ne!(workspace.revision(), meta_changed_revision);
    let external_changed_revision = workspace.revision();
    let external_changed = assert_incremental_matches_fresh(&workspace, &paths, initial_layout, 3);
    let rebound_binary =
        canonical_binary_fact(&external_changed, INCREMENTAL_BINARY_ALIAS, 2, "$.m_Father");
    assert!(matches!(
        &rebound_binary.raw_target,
        RawReferenceTarget::Binary {
            file_id: 1,
            path_id: 1,
            external: Some(external),
        } if external.guid() == Some([0x22; 16])
    ));
    assert!(matches!(
        rebound_binary.resolution,
        CanonicalResolution::Resolved(_)
    ));

    load_with_alias(&mut workspace, &paths.target_meta, REMAPPED_META_ALIAS);
    let remapped_target = load_with_alias(&mut workspace, &paths.target, REMAPPED_TARGET_ALIAS);
    assert_ne!(workspace.revision(), external_changed_revision);
    let remapped_revision = workspace.revision();
    let remapped_layout = IncrementalParityLayout {
        target_alias: REMAPPED_TARGET_ALIAS,
        meta_alias: REMAPPED_META_ALIAS,
        target_loaded: true,
    };
    let remapped = assert_incremental_matches_fresh(&workspace, &paths, remapped_layout, 4);
    for fact in [
        canonical_fact(&remapped, INCREMENTAL_OWNER_ALIAS, "$.m_Target"),
        canonical_binary_fact(&remapped, INCREMENTAL_BINARY_ALIAS, 2, "$.m_Father"),
    ] {
        assert!(matches!(
            fact.resolution,
            CanonicalResolution::Resolved(ref target)
                if target.source.root_alias().as_str() == REMAPPED_TARGET_ALIAS
        ));
    }

    workspace
        .unload_source(remapped_target, &mut AssetLoadBudget::default())
        .unwrap();
    assert_ne!(workspace.revision(), remapped_revision);
    let unloaded_revision = workspace.revision();
    let unloaded_layout = IncrementalParityLayout {
        target_alias: REMAPPED_TARGET_ALIAS,
        meta_alias: REMAPPED_META_ALIAS,
        target_loaded: false,
    };
    let unloaded = assert_incremental_matches_fresh(&workspace, &paths, unloaded_layout, 3);
    for fact in [
        canonical_fact(&unloaded, INCREMENTAL_OWNER_ALIAS, "$.m_Target"),
        canonical_binary_fact(&unloaded, INCREMENTAL_BINARY_ALIAS, 2, "$.m_Father"),
    ] {
        assert!(matches!(fact.resolution, CanonicalResolution::Unloaded(_)));
    }

    load_with_alias(&mut workspace, &paths.target, REMAPPED_TARGET_ALIAS);
    assert_ne!(workspace.revision(), unloaded_revision);
    let reloaded = assert_incremental_matches_fresh(&workspace, &paths, remapped_layout, 3);
    for fact in [
        canonical_fact(&reloaded, INCREMENTAL_OWNER_ALIAS, "$.m_Target"),
        canonical_binary_fact(&reloaded, INCREMENTAL_BINARY_ALIAS, 2, "$.m_Father"),
    ] {
        assert!(matches!(
            fact.resolution,
            CanonicalResolution::Resolved(ref target)
                if target.source.root_alias().as_str() == REMAPPED_TARGET_ALIAS
        ));
    }
}
