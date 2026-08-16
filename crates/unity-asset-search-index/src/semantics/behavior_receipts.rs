//! Deterministic behavior receipts for persisted search semantics.
//!
//! The receipts execute the real analyzer and projection algorithms against a fixed workspace
//! fixture. They hash existing serialized domain results directly, so this test does not maintain
//! a second shadow model of analysis or projection semantics. Runtime and allocation metrics are
//! intentionally excluded: they are operational evidence, not persisted search behavior.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tempfile::tempdir;
use unity_asset::reference::{ReferenceGraph, ReferenceGraphBuildOptions};
use unity_asset::workspace::{AssetWorkspace, SourceOpenRequest, WorkspaceOptions};
use unity_asset::{
    AssetLoadBudget, BudgetedSourceBytes, Diagnostic, DigestV1, SourceAlias, SourceId, SourceKind,
    WorkspaceId,
};
use unity_asset_search_core::SearchKind;

use crate::analysis::{
    AnalysisMetrics, AnalyzedSource, AssetAnalysis, AssetAnalysisBatch, ReferenceProjectionFact,
    SearchFacts,
};
use crate::analyzer::{AnalyzedAsset, AnalyzerLimits, AssetAnalyzer, WorkspaceAnalysisContext};
use crate::projection::{
    GenerationProjection, ProjectionCategory, ProjectionLimits, ProjectionTruncation,
    ReferenceDocument, SearchDocument, project_batch,
};
use crate::scan::{FileHint, ReadSource, SourceHints};
use crate::semantics::{
    ANALYSIS_BEHAVIOR_RECEIPT, REFERENCE_PROJECTION_BEHAVIOR_RECEIPT,
    SEARCH_PROJECTION_BEHAVIOR_RECEIPT,
};
use crate::source_coordinate::IndexedSourceCoordinate;

const ANALYSIS_RECEIPT_SCHEMA: &str = "unity-asset.search.analysis-behavior-receipt.v1";
const SEARCH_RECEIPT_SCHEMA: &str = "unity-asset.search.search-projection-behavior-receipt.v1";
const REFERENCE_RECEIPT_SCHEMA: &str =
    "unity-asset.search.reference-projection-behavior-receipt.v1";
const SCRIPT_GUID: &str = "00112233445566778899aabbccddeeff";

#[derive(Serialize)]
struct AnalysisBehaviorReceipt<'a> {
    schema: &'static str,
    assets: Vec<&'a AssetAnalysis>,
}

#[derive(Serialize)]
struct SearchProjectionBehaviorReceipt<'a> {
    schema: &'static str,
    documents: &'a [SearchDocument],
    diagnostics: &'a [Diagnostic],
    truncations: Vec<&'a ProjectionTruncation>,
}

#[derive(Serialize)]
struct ReferenceProjectionBehaviorReceipt<'a> {
    schema: &'static str,
    documents: &'a [ReferenceDocument],
    diagnostics: &'a [Diagnostic],
    truncations: Vec<&'a ProjectionTruncation>,
}

struct Fixture {
    analyses: Vec<AnalyzedAsset>,
    projection: GenerationProjection,
}

#[test]
fn production_semantics_match_real_algorithm_behavior_receipts() {
    let first = build_fixture();
    let second = build_fixture();
    let first_receipts = receipts(&first);
    let second_receipts = receipts(&second);
    assert_eq!(
        first_receipts, second_receipts,
        "the behavior fixture must be independent of temporary paths and allocation order"
    );

    assert_eq!(
        [first_receipts.0, first_receipts.1, first_receipts.2],
        [
            ANALYSIS_BEHAVIOR_RECEIPT,
            SEARCH_PROJECTION_BEHAVIOR_RECEIPT,
            REFERENCE_PROJECTION_BEHAVIOR_RECEIPT,
        ],
        "update a frozen receipt only after reviewing the corresponding behavior change"
    );
}

fn receipts(fixture: &Fixture) -> (DigestV1, DigestV1, DigestV1) {
    fixture
        .analyses
        .iter()
        .for_each(|asset| bind_asset_analysis_shape(&asset.analysis));
    bind_generation_projection_shape(&fixture.projection);
    let analyses = fixture
        .analyses
        .iter()
        .map(|asset| &asset.analysis)
        .collect();
    let analysis = AnalysisBehaviorReceipt {
        schema: ANALYSIS_RECEIPT_SCHEMA,
        assets: analyses,
    };
    let search = SearchProjectionBehaviorReceipt {
        schema: SEARCH_RECEIPT_SCHEMA,
        documents: &fixture.projection.search_documents,
        diagnostics: &fixture.projection.diagnostics,
        truncations: fixture
            .projection
            .truncations
            .iter()
            .filter(|truncation| truncation.category == ProjectionCategory::ContainerEntries)
            .collect(),
    };
    let references = ReferenceProjectionBehaviorReceipt {
        schema: REFERENCE_RECEIPT_SCHEMA,
        documents: &fixture.projection.reference_documents,
        diagnostics: &fixture.projection.diagnostics,
        truncations: fixture
            .projection
            .truncations
            .iter()
            .filter(|truncation| truncation.category == ProjectionCategory::References)
            .collect(),
    };
    (
        digest_json(&analysis),
        digest_json(&search),
        digest_json(&references),
    )
}

fn digest_json<T: Serialize>(value: &T) -> DigestV1 {
    let encoded = serde_json::to_vec(value).expect("behavior receipt must serialize");
    DigestV1::hash_bytes(&encoded)
}

fn bind_asset_analysis_shape(analysis: &AssetAnalysis) {
    let AssetAnalysis {
        source,
        search,
        graph_inputs,
        references,
        container_entries,
        diagnostics,
        truncations,
        complete,
    } = analysis;
    bind_analyzed_source_shape(source);
    bind_search_facts_shape(search);
    references.iter().for_each(bind_reference_fact_shape);
    let _ = (
        graph_inputs,
        container_entries,
        diagnostics,
        truncations,
        complete,
    );
}

fn bind_analyzed_source_shape(source: &AnalyzedSource) {
    let AnalyzedSource {
        coordinate,
        relative_path,
        content_digest,
        length,
        search_kind,
        guid,
        workspace_source,
        workspace_fingerprint,
        locator,
    } = source;
    let _ = (
        coordinate,
        relative_path,
        content_digest,
        length,
        search_kind,
        guid,
        workspace_source,
        workspace_fingerprint,
        locator,
    );
}

fn bind_search_facts_shape(search: &SearchFacts) {
    let SearchFacts {
        display_name,
        path_terms,
        name_terms,
        content_terms,
        hierarchy_paths,
        script_symbols,
        referenced_script_guids,
    } = search;
    let _ = (
        display_name,
        path_terms,
        name_terms,
        content_terms,
        hierarchy_paths,
        script_symbols,
        referenced_script_guids,
    );
}

fn bind_reference_fact_shape(fact: &ReferenceProjectionFact) {
    let ReferenceProjectionFact {
        source_object,
        source_class_id,
        field_path,
        raw_target,
        resolution,
        diagnostics,
        dependency_keys,
    } = fact;
    let _ = (
        source_object,
        source_class_id,
        field_path,
        raw_target,
        resolution,
        diagnostics,
        dependency_keys,
    );
}

fn bind_generation_projection_shape(projection: &GenerationProjection) {
    let GenerationProjection {
        search_documents,
        reference_documents,
        diagnostics,
        truncations,
        metrics,
    } = projection;
    search_documents.iter().for_each(bind_search_document_shape);
    reference_documents
        .iter()
        .for_each(bind_reference_document_shape);
    let _ = (diagnostics, truncations, metrics);
}

fn bind_search_document_shape(document: &SearchDocument) {
    let SearchDocument {
        stable_id,
        guid,
        path,
        path_terms,
        name,
        name_terms,
        kind,
        kind_terms,
        content_terms,
        hierarchy_paths,
        script_symbols,
        container_source_path,
    } = document;
    let _ = (
        stable_id,
        guid,
        path,
        path_terms,
        name,
        name_terms,
        kind,
        kind_terms,
        content_terms,
        hierarchy_paths,
        script_symbols,
        container_source_path,
    );
}

fn bind_reference_document_shape(document: &ReferenceDocument) {
    let ReferenceDocument {
        stable_id,
        source_path,
        source_kind,
        source_guid,
        fact,
        incoming_keys,
        outgoing_keys,
    } = document;
    bind_reference_fact_shape(fact);
    let _ = (
        stable_id,
        source_path,
        source_kind,
        source_guid,
        incoming_keys,
        outgoing_keys,
    );
}

fn build_fixture() -> Fixture {
    let workspace_id = WorkspaceId::from_u128(0x1234_5678_9abc_def0).unwrap();
    let directory = tempdir().unwrap();
    let prefab_path = directory.path().join("Hero.prefab");
    let prefab_payload: Arc<[u8]> = Arc::from(PREFAB.as_bytes());
    std::fs::write(&prefab_path, prefab_payload.as_ref()).unwrap();

    let mut workspace =
        AssetWorkspace::with_workspace_id(workspace_id, WorkspaceOptions::default()).unwrap();
    let mut workspace_budget = AssetLoadBudget::default();
    let root = workspace
        .load_source_bytes(
            SourceOpenRequest::new(
                &prefab_path,
                SourceAlias::new("Assets/Hero.prefab".to_owned()).unwrap(),
            )
            .with_kind_hint(SourceKind::Yaml),
            Arc::clone(&prefab_payload),
            &mut workspace_budget,
        )
        .unwrap();
    let snapshot = workspace.snapshot();
    let graph = ReferenceGraph::build(
        &snapshot,
        ReferenceGraphBuildOptions::unbounded(),
        &mut workspace_budget,
    )
    .unwrap();
    let context =
        WorkspaceAnalysisContext::build(&snapshot, &graph, &mut workspace_budget).unwrap();

    let analyzer = AssetAnalyzer::new(AnalyzerLimits::default());
    let mut prefab_budget = AssetLoadBudget::default();
    let prefab = analyzer
        .analyze(
            &read_source(
                root,
                "Assets/Hero.prefab",
                SearchKind::Prefab,
                None,
                Some(Arc::clone(&prefab_payload)),
                &mut prefab_budget,
            ),
            Some(context.asset(root).unwrap()),
            &mut prefab_budget,
        )
        .unwrap();

    let script_bytes: Arc<[u8]> =
        Arc::from(b"namespace Example.Game;\npublic sealed class HeroController {}\n".as_slice());
    let mut script_budget = AssetLoadBudget::default();
    let script_source = SourceId::new(workspace_id, SourceKind::SerializedFile, 2).unwrap();
    let script = analyzer
        .analyze(
            &read_source(
                script_source,
                "Assets/Scripts/HeroController.cs",
                SearchKind::Script,
                Some(SCRIPT_GUID.to_owned()),
                Some(script_bytes),
                &mut script_budget,
            ),
            None,
            &mut script_budget,
        )
        .unwrap();

    let mut metrics = AnalysisMetrics::default();
    metrics.merge(&prefab.metrics);
    metrics.merge(&script.metrics);
    let batch = AssetAnalysisBatch::new(
        workspace_id,
        snapshot.revision(),
        Vec::new(),
        vec![prefab.analysis.clone(), script.analysis.clone()],
        metrics,
    );
    let mut projection_budget = AssetLoadBudget::default();
    let projection = project_batch(
        &batch,
        ProjectionLimits {
            max_references_per_asset: 256,
            max_container_entries_per_asset: 256,
        },
        &mut projection_budget,
    )
    .unwrap();
    Fixture {
        analyses: vec![prefab, script],
        projection,
    }
}

fn read_source(
    source: SourceId,
    relative_path: &str,
    kind: SearchKind,
    guid: Option<String>,
    bytes: Option<Arc<[u8]>>,
    budget: &mut AssetLoadBudget,
) -> ReadSource {
    let length = bytes
        .as_ref()
        .map_or(0, |value| u64::try_from(value.len()).unwrap());
    let content_identity = bytes.as_deref().map_or_else(
        || DigestV1::hash_bytes(relative_path.as_bytes()),
        DigestV1::hash_bytes,
    );
    let bytes = bytes.map(|value| BudgetedSourceBytes::from_arc(value, budget).unwrap());
    ReadSource {
        coordinate: IndexedSourceCoordinate::workspace(source),
        rel_path: relative_path.to_owned(),
        abs_path: PathBuf::from(relative_path),
        name: relative_path
            .rsplit('/')
            .next()
            .unwrap_or(relative_path)
            .to_owned(),
        kind,
        guid,
        bytes,
        meta_bytes: None,
        length,
        content_identity,
        hints: SourceHints {
            asset: FileHint {
                size: length,
                mtime_ms: None,
            },
            meta: None,
        },
        unchanged: false,
    }
}

const PREFAB: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_ObjectHideFlags: 0
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  serializedVersion: 6
  m_Component:
  - component: {fileID: 200}
  - component: {fileID: 300}
  m_Layer: 0
  m_Name: Hero
  m_TagString: Untagged
  m_Icon: {fileID: 0}
  m_NavMeshLayer: 0
  m_StaticEditorFlags: 0
  m_IsActive: 1
--- !u!4 &200
Transform:
  m_ObjectHideFlags: 0
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_GameObject: {fileID: 100}
  serializedVersion: 2
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Children: []
  m_Father: {fileID: 0}
  m_RootOrder: 0
--- !u!114 &300
MonoBehaviour:
  m_ObjectHideFlags: 0
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  m_GameObject: {fileID: 100}
  m_Enabled: 1
  m_EditorHideFlags: 0
  m_Script: {fileID: 11500000, guid: 00112233445566778899aabbccddeeff, type: 3}
  m_Name: HeroController
  m_EditorClassIdentifier:
"#;
