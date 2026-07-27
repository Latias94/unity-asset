use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use unity_asset_core::{Diagnostic, WorkspaceId, WorkspaceRevision};
use unity_asset_search_core::{
    FuzzyWorkUsage, HighlightRange, MatchCount, MatchExplanation, MatchKind, RankingSignals,
    SearchDiagnostic,
};
use unity_asset_search_index::{
    ApiErrorCode, AssetLoadBudget, GenerationStamp, IndexPaths, Location, ReferenceContext,
    ReferenceCoverage, ReferenceHit, ReferenceObject, ReferenceRequest, ReferencesResponse,
    ReindexDisposition, ReindexIntent, ReindexReceipt, SearchCapabilities, SearchHit, SearchIndex,
    SearchIndexOptions, SearchRequest, SearchResponse, StatusResponse, SuggestResponse,
};

const PREFAB_GUID: &str = "fedcba98765432100123456789abcdef";
const SCRIPT_GUID: &str = "00112233445566778899aabbccddeeff";
const INLINE_GUID: &str = "11112222333344445555666677778888";
const BLOCK_GUID: &str = "aaaabbbbccccddddeeeeffff00001111";
const HERO_PATH: &str = "Assets/Characters/Hero.prefab";
const SCRIPT_PATH: &str = "Assets/Scripts/HeroController.cs";

const DRAFT_PREFAB: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_ObjectHideFlags: 0
  m_Component:
  - component: {fileID: -42}
  m_Name: DraftHero
--- !u!114 &-42
MonoBehaviour:
  m_ObjectHideFlags: 0
  m_GameObject: {fileID: 1}
  m_Script: {fileID: 0}
  m_Name:
"#;

const FINAL_PREFAB: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_ObjectHideFlags: 0
  m_Component:
  - component: {fileID: -42}
  m_Name: FinalHero
--- !u!114 &-42
MonoBehaviour:
  m_ObjectHideFlags: 0
  m_GameObject: {fileID: 1}
  # Comments and whitespace are semantic noise around a structured PPtr.
  m_Script: {
    fileID: -11500000,
    guid: 00112233445566778899aabbccddeeff,
    type: 3
  } # trailing comment
  m_InlineTarget: { fileID: -17, guid: 11112222333344445555666677778888, type: 2 }
  m_BlockTarget:
    fileID: -23
    guid: aaaabbbbccccddddeeeeffff00001111
    type: 3
  m_Text: |
    This scalar is not a PPtr: {fileID: -999, guid: 00112233445566778899aabbccddeeff, type: 3}
  m_Name:
"#;

const DRAFT_SCRIPT: &str = "namespace Draft.Game;\npublic sealed class DraftController {}\n";
const FINAL_SCRIPT: &str = "namespace Final.Game;\npublic sealed class FinalController {}\n";

#[derive(Debug, Clone, Copy)]
enum ProjectVersion {
    Draft,
    Final,
}

struct ProjectFixture {
    temporary: TempDir,
    hero_path: PathBuf,
    script_path: PathBuf,
}

impl ProjectFixture {
    fn new(version: ProjectVersion) -> Self {
        let temporary = TempDir::new().unwrap();
        let hero_path = temporary.path().join(HERO_PATH);
        let script_path = temporary.path().join(SCRIPT_PATH);
        fs::create_dir_all(hero_path.parent().unwrap()).unwrap();
        fs::create_dir_all(script_path.parent().unwrap()).unwrap();
        fs::write(
            hero_path.with_extension("prefab.meta"),
            format!("fileFormatVersion: 2\nguid: {PREFAB_GUID}\n"),
        )
        .unwrap();
        fs::write(
            script_path.with_extension("cs.meta"),
            format!("fileFormatVersion: 2\nguid: {SCRIPT_GUID}\n"),
        )
        .unwrap();

        let fixture = Self {
            temporary,
            hero_path,
            script_path,
        };
        fixture.write_version(version);
        fixture
    }

    fn root(&self) -> &Path {
        self.temporary.path()
    }

    fn assets_directory(&self) -> PathBuf {
        self.root().join("Assets")
    }

    fn write_version(&self, version: ProjectVersion) {
        let (prefab, script) = match version {
            ProjectVersion::Draft => (DRAFT_PREFAB, DRAFT_SCRIPT),
            ProjectVersion::Final => (FINAL_PREFAB, FINAL_SCRIPT),
        };
        fs::write(&self.hero_path, prefab).unwrap();
        fs::write(&self.script_path, script).unwrap();
    }

    fn index_paths(&self) -> IndexPaths {
        self.index_paths_with(".search-index", vec![PathBuf::from("Assets")])
    }

    fn index_paths_with(&self, index_directory: &str, scan_roots: Vec<PathBuf>) -> IndexPaths {
        IndexPaths::for_project(
            self.root().to_path_buf(),
            Some(self.root().join(index_directory)),
            Some(scan_roots),
        )
        .unwrap()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct LocationFact {
    path: String,
    guid: Option<String>,
    file_id: Option<i64>,
    class_id: Option<i32>,
}

impl From<Location> for LocationFact {
    fn from(location: Location) -> Self {
        Self {
            path: location.path,
            guid: location.guid,
            file_id: location.file_id,
            class_id: location.class_id,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SearchHitFact {
    rank: usize,
    guid: Option<String>,
    path: String,
    name: String,
    kind: String,
    stable_id: String,
    location: LocationFact,
    ranking_signals: RankingSignals,
    match_kind: MatchKind,
    explanation: MatchExplanation,
    matched_hierarchy_paths: Vec<String>,
    matched_script_symbols: Vec<String>,
    highlight_path_ranges: Vec<HighlightRange>,
    highlight_name_ranges: Vec<HighlightRange>,
    highlight_path: Option<String>,
    highlight_name: Option<String>,
}

impl From<SearchHit> for SearchHitFact {
    fn from(hit: SearchHit) -> Self {
        Self {
            rank: hit.rank,
            guid: hit.guid,
            path: hit.path,
            name: hit.name,
            kind: hit.kind,
            stable_id: hit.stable_id,
            location: hit.location.into(),
            ranking_signals: hit.ranking_signals,
            match_kind: hit.match_kind,
            explanation: hit.explanation,
            matched_hierarchy_paths: hit.matched_hierarchy_paths,
            matched_script_symbols: hit.matched_script_symbols,
            highlight_path_ranges: hit.highlight_path_ranges,
            highlight_name_ranges: hit.highlight_name_ranges,
            highlight_path: hit.highlight_path,
            highlight_name: hit.highlight_name,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SearchResponseFact {
    contract_version: u16,
    query: String,
    match_count: MatchCount,
    returned_hits: usize,
    request_limit_truncated: bool,
    fuzzy_work: FuzzyWorkUsage,
    hits: Vec<SearchHitFact>,
    diagnostics: Vec<SearchDiagnostic>,
    fallback_used: bool,
}

impl From<SearchResponse> for SearchResponseFact {
    fn from(response: SearchResponse) -> Self {
        Self {
            contract_version: response.contract_version,
            query: response.query,
            match_count: response.match_count,
            returned_hits: response.returned_hits,
            request_limit_truncated: response.request_limit_truncated,
            fuzzy_work: response.fuzzy_work,
            hits: response.hits.into_iter().map(Into::into).collect(),
            diagnostics: response.diagnostics,
            fallback_used: response.fallback_used,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ReferenceContextFact {
    doc_file_id: Option<i64>,
    doc_class_id: Option<i32>,
    object_name: Option<String>,
    hierarchy_path: Option<String>,
    field_hint: Option<String>,
    source_line: Option<u32>,
    source_column: Option<u32>,
}

impl From<ReferenceContext> for ReferenceContextFact {
    fn from(context: ReferenceContext) -> Self {
        Self {
            doc_file_id: context.doc_file_id,
            doc_class_id: context.doc_class_id,
            object_name: context.object_name,
            hierarchy_path: context.hierarchy_path,
            field_hint: context.field_hint,
            source_line: context.source_line,
            source_column: context.source_column,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ReferenceObjectFact {
    doc_file_id: Option<i64>,
    doc_class_id: Option<i32>,
    stable_id: String,
    location: LocationFact,
    object_name: Option<String>,
    hierarchy_path: Option<String>,
    field_hints: Vec<String>,
}

impl From<ReferenceObject> for ReferenceObjectFact {
    fn from(object: ReferenceObject) -> Self {
        Self {
            doc_file_id: object.doc_file_id,
            doc_class_id: object.doc_class_id,
            stable_id: object.stable_id,
            location: object.location.into(),
            object_name: object.object_name,
            hierarchy_path: object.hierarchy_path,
            field_hints: object.field_hints,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ReferenceHitFact {
    source_path: String,
    source_kind: String,
    stable_id: String,
    location: LocationFact,
    contexts: Vec<ReferenceContextFact>,
    objects: Vec<ReferenceObjectFact>,
}

impl From<ReferenceHit> for ReferenceHitFact {
    fn from(hit: ReferenceHit) -> Self {
        Self {
            source_path: hit.source_path,
            source_kind: hit.source_kind,
            stable_id: hit.stable_id,
            location: hit.location.into(),
            contexts: hit.contexts.into_iter().map(Into::into).collect(),
            objects: hit.objects.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ReferenceResponseFact {
    contract_version: u16,
    request: ReferenceRequest,
    coverage: ReferenceCoverage,
    hits: Vec<ReferenceHitFact>,
    diagnostics: Vec<Diagnostic>,
}

impl From<ReferencesResponse> for ReferenceResponseFact {
    fn from(response: ReferencesResponse) -> Self {
        Self {
            contract_version: response.contract_version,
            request: response.request,
            coverage: response.coverage,
            hits: response.hits.into_iter().map(Into::into).collect(),
            diagnostics: response.diagnostics,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SuggestionFact {
    contract_version: u16,
    prefix: String,
    suggestions: Vec<String>,
}

impl From<SuggestResponse> for SuggestionFact {
    fn from(response: SuggestResponse) -> Self {
        Self {
            contract_version: response.contract_version,
            prefix: response.prefix,
            suggestions: response.suggestions,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct StatusFact {
    contract_version: u16,
    workspace: WorkspaceId,
    actual_revision: WorkspaceRevision,
    desired_revision: WorkspaceRevision,
    stale: bool,
    capabilities: SearchCapabilities,
    project_root: PathBuf,
    generation_root: PathBuf,
    scan_roots: Vec<PathBuf>,
    indexed_assets: u64,
    indexed_search_documents: u64,
    indexed_reference_facts: u64,
    incomplete_assets: u64,
    projection_truncations: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct PublicGenerationFacts {
    status: StatusFact,
    hero_search: SearchResponseFact,
    controller_search: SearchResponseFact,
    references: Vec<ReferenceResponseFact>,
    suggestions: SuggestionFact,
}

#[derive(Debug, PartialEq, Eq)]
struct PublicSemanticFacts<'facts> {
    contract_version: u16,
    stale: bool,
    capabilities: &'facts SearchCapabilities,
    project_root: &'facts Path,
    indexed_assets: u64,
    indexed_search_documents: u64,
    indexed_reference_facts: u64,
    incomplete_assets: u64,
    projection_truncations: u64,
    hero_search: &'facts SearchResponseFact,
    controller_search: &'facts SearchResponseFact,
    references: &'facts [ReferenceResponseFact],
    suggestions: &'facts SuggestionFact,
}

impl PublicGenerationFacts {
    fn semantic(&self) -> PublicSemanticFacts<'_> {
        PublicSemanticFacts {
            contract_version: self.status.contract_version,
            stale: self.status.stale,
            capabilities: &self.status.capabilities,
            project_root: &self.status.project_root,
            indexed_assets: self.status.indexed_assets,
            indexed_search_documents: self.status.indexed_search_documents,
            indexed_reference_facts: self.status.indexed_reference_facts,
            incomplete_assets: self.status.incomplete_assets,
            projection_truncations: self.status.projection_truncations,
            hero_search: &self.hero_search,
            controller_search: &self.controller_search,
            references: &self.references,
            suggestions: &self.suggestions,
        }
    }
}

fn open_index(fixture: &ProjectFixture) -> SearchIndex {
    let mut budget = AssetLoadBudget::default();
    SearchIndex::open_or_create(fixture.index_paths(), &mut budget).unwrap()
}

fn open_index_with(
    fixture: &ProjectFixture,
    index_directory: &str,
    scan_roots: Vec<PathBuf>,
) -> SearchIndex {
    let mut budget = AssetLoadBudget::default();
    SearchIndex::open_or_create(
        fixture.index_paths_with(index_directory, scan_roots),
        &mut budget,
    )
    .unwrap()
}

fn reindex(index: &SearchIndex, intent: ReindexIntent) -> ReindexReceipt {
    let mut budget = AssetLoadBudget::default();
    index.reindex(intent, &mut budget).unwrap()
}

fn published_stamp(index: &SearchIndex, receipt: &ReindexReceipt) -> GenerationStamp {
    assert_eq!(receipt.disposition, ReindexDisposition::Applied);
    assert!(receipt.transaction.is_none());
    assert!(receipt.target_revision.is_none());
    assert!(receipt.evidence.disk_estimate.is_some());
    assert!(receipt.evidence.analysis.assets_visited > 0);
    assert!(receipt.evidence.analysis.assets_analyzed > 0);
    assert!(receipt.evidence.analysis.source_opens > 0);
    assert!(receipt.evidence.analysis.source_bytes_read > 0);

    let generation = receipt
        .generation
        .clone()
        .expect("a successful filesystem publication returns its generation");
    let status = index.status().unwrap();
    assert_eq!(status.generation.active.as_ref(), Some(&generation));
    assert_eq!(generation.actual_revision, generation.desired_revision);
    assert!(!generation.stale);
    assert_eq!(status.generation.building_revision, None);
    assert!(status.generation.last_failure.is_none());
    assert!(!status.indexing);
    assert!(status.progress.is_none());
    generation
}

fn capture_search(
    index: &SearchIndex,
    active: &GenerationStamp,
    query: &str,
) -> SearchResponseFact {
    let response = index.search(SearchRequest::new(query, 20)).unwrap();
    assert_eq!(&response.generation, active);
    response.into()
}

fn capture_references(
    index: &SearchIndex,
    active: &GenerationStamp,
    request: ReferenceRequest,
) -> ReferenceResponseFact {
    let response = index
        .references(request, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(&response.generation, active);
    response.into()
}

fn capture_public_facts(index: &SearchIndex) -> PublicGenerationFacts {
    let status = index.status().unwrap();
    assert!(!status.indexing);
    assert!(status.progress.is_none());
    assert!(status.generation.last_failure.is_none());
    let active = status
        .generation
        .active
        .clone()
        .expect("the final project has an active generation");

    let hero_search = capture_search(index, &active, "FinalHero");
    let controller_search = capture_search(index, &active, "FinalController");
    let references = [SCRIPT_GUID, INLINE_GUID, BLOCK_GUID]
        .into_iter()
        .map(|guid| {
            capture_references(
                index,
                &active,
                ReferenceRequest::incoming_guid(guid, None, 20),
            )
        })
        .collect();
    let suggestions = index.suggest("in:Assets/", 20).unwrap();
    assert_eq!(suggestions.generation, active);

    let status = StatusFact {
        contract_version: status.contract_version,
        workspace: active.workspace,
        actual_revision: active.actual_revision,
        desired_revision: active.desired_revision,
        stale: active.stale,
        capabilities: status.capabilities,
        project_root: status.project_root,
        generation_root: status.generation_root,
        scan_roots: status.scan_roots,
        indexed_assets: status.indexed_assets,
        indexed_search_documents: status.indexed_search_documents,
        indexed_reference_facts: status.indexed_reference_facts,
        incomplete_assets: status.incomplete_assets,
        projection_truncations: status.projection_truncations,
    };

    PublicGenerationFacts {
        status,
        hero_search,
        controller_search,
        references,
        suggestions: suggestions.into(),
    }
}

fn assert_single_yaml_reference(
    response: &ReferenceResponseFact,
    expected_guid: &str,
    expected_file_id: i64,
    expected_field: &str,
) {
    assert_eq!(
        response.coverage,
        ReferenceCoverage {
            complete: true,
            truncated: false,
            returned: 1,
            total: Some(1),
            next_cursor: None,
        }
    );
    let hit = response.hits.first().unwrap();
    assert_eq!(hit.source_path, HERO_PATH);
    assert_eq!(hit.source_kind, "Prefab");
    assert_eq!(hit.location.path, HERO_PATH);
    assert_eq!(hit.location.guid.as_deref(), Some(PREFAB_GUID));
    assert_eq!(hit.location.file_id, Some(-42));
    assert_eq!(hit.location.class_id, Some(114));
    assert!(hit.contexts.iter().any(|context| {
        context.doc_file_id == Some(-42)
            && context.doc_class_id == Some(114)
            && context
                .field_hint
                .as_deref()
                .is_some_and(|field| field.contains(expected_field))
    }));

    let file_hint = format!("raw.yaml.file_id={expected_file_id}");
    let raw_target = hit
        .objects
        .iter()
        .find(|object| object.field_hints.iter().any(|hint| hint == &file_hint))
        .expect("the projected hit exposes the structured raw YAML target");
    assert_eq!(raw_target.doc_file_id, Some(expected_file_id));
    assert_eq!(raw_target.location.file_id, Some(expected_file_id));
    assert_eq!(raw_target.location.guid.as_deref(), Some(expected_guid));
}

fn active_stamp(status: &StatusResponse) -> GenerationStamp {
    status
        .generation
        .active
        .clone()
        .expect("the index has an active generation")
}

fn assert_version_is_queryable(
    index: &SearchIndex,
    active: &GenerationStamp,
    present_name: &str,
    absent_name: &str,
) {
    let status_before = serde_json::to_value(index.status().unwrap()).unwrap();
    let present = capture_search(index, active, present_name);
    assert_eq!(present.returned_hits, 1);
    assert_eq!(present.hits[0].path, HERO_PATH);

    let absent = capture_search(index, active, absent_name);
    assert_eq!(absent.returned_hits, 0);
    assert!(absent.hits.is_empty());
    let status_after = serde_json::to_value(index.status().unwrap()).unwrap();
    assert_eq!(
        status_after, status_before,
        "published queries must not mutate generation or runtime status"
    );
}

#[test]
fn full_reconcile_and_changed_paths_converge_on_public_generation_facts() {
    let fixture = ProjectFixture::new(ProjectVersion::Final);
    let index = open_index(&fixture);

    let full_receipt = reindex(&index, ReindexIntent::full());
    assert!(full_receipt.evidence.forced_full_scan);
    let full_stamp = published_stamp(&index, &full_receipt);
    assert_version_is_queryable(&index, &full_stamp, "FinalHero", "DraftHero");
    let full = capture_public_facts(&index);

    fixture.write_version(ProjectVersion::Draft);
    let reconcile_draft_receipt = reindex(&index, ReindexIntent::full());
    let reconcile_draft_stamp = published_stamp(&index, &reconcile_draft_receipt);
    assert_ne!(
        reconcile_draft_stamp.generation, full_stamp.generation,
        "the Draft publication must create a new generation"
    );
    assert_version_is_queryable(&index, &reconcile_draft_stamp, "DraftHero", "FinalHero");

    fixture.write_version(ProjectVersion::Final);
    let reconcile_receipt = reindex(&index, ReindexIntent::reconcile());
    assert!(!reconcile_receipt.evidence.forced_full_scan);
    let reconcile_stamp = published_stamp(&index, &reconcile_receipt);
    assert_ne!(
        reconcile_stamp.generation, reconcile_draft_stamp.generation,
        "reconciling Final must replace the Draft generation"
    );
    assert_version_is_queryable(&index, &reconcile_stamp, "FinalHero", "DraftHero");
    let reconcile = capture_public_facts(&index);

    fixture.write_version(ProjectVersion::Draft);
    let changed_draft_receipt = reindex(&index, ReindexIntent::full());
    let changed_draft_stamp = published_stamp(&index, &changed_draft_receipt);
    assert_ne!(
        changed_draft_stamp.generation, reconcile_stamp.generation,
        "the second Draft publication must create a new generation"
    );
    assert_version_is_queryable(&index, &changed_draft_stamp, "DraftHero", "FinalHero");

    fixture.write_version(ProjectVersion::Final);
    let changed_receipt = reindex(
        &index,
        ReindexIntent::changed_paths(vec![PathBuf::from(HERO_PATH), PathBuf::from(SCRIPT_PATH)]),
    );
    assert!(!changed_receipt.evidence.forced_full_scan);
    let changed_stamp = published_stamp(&index, &changed_receipt);
    assert_ne!(
        changed_stamp.generation, changed_draft_stamp.generation,
        "the changed-path publication must replace the Draft generation"
    );
    assert_version_is_queryable(&index, &changed_stamp, "FinalHero", "DraftHero");
    let changed = capture_public_facts(&index);

    assert_eq!(full, reconcile);
    assert_eq!(full, changed);
}

#[test]
fn unchanged_tier_zero_asset_is_reanalyzed_when_source_retention_increases() {
    let fixture = ProjectFixture::new(ProjectVersion::Draft);
    let paths = fixture.index_paths_with(
        ".retained-source-transition-index",
        vec![PathBuf::from("Assets")],
    );
    let tier_zero_options = SearchIndexOptions {
        max_retained_source_bytes: 1,
        ..SearchIndexOptions::default()
    };
    let tier_zero = SearchIndex::open_or_create_with_options(
        paths.clone(),
        tier_zero_options,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();

    let tier_zero_receipt = tier_zero
        .reindex(ReindexIntent::full(), &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(tier_zero_receipt.disposition, ReindexDisposition::Applied);
    assert!(tier_zero.status().unwrap().incomplete_assets > 0);
    assert!(
        tier_zero
            .search(SearchRequest::new("DraftHero", 20))
            .unwrap()
            .hits
            .is_empty()
    );
    drop(tier_zero);

    let hydrated = SearchIndex::open_or_create_with_options(
        paths,
        SearchIndexOptions::default(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let hydrated_receipt = hydrated
        .reindex(ReindexIntent::full(), &mut AssetLoadBudget::default())
        .unwrap();

    assert_eq!(hydrated_receipt.disposition, ReindexDisposition::Applied);
    assert!(hydrated_receipt.evidence.analysis.assets_analyzed > 0);
    assert_eq!(hydrated.status().unwrap().incomplete_assets, 0);
    assert_eq!(
        hydrated
            .search(SearchRequest::new("DraftHero", 20))
            .unwrap()
            .hits
            .into_iter()
            .map(|hit| hit.path)
            .collect::<Vec<_>>(),
        vec![HERO_PATH.to_owned()]
    );
}

#[test]
fn non_overlapping_scan_root_shards_match_a_single_project_scan_root() {
    let fixture = ProjectFixture::new(ProjectVersion::Final);
    let single = open_index_with(
        &fixture,
        ".single-root-index",
        vec![PathBuf::from("Assets")],
    );
    let sharded = open_index_with(
        &fixture,
        ".sharded-root-index",
        vec![
            PathBuf::from("Assets/Characters"),
            PathBuf::from("Assets/Scripts"),
        ],
    );

    let single_receipt = reindex(&single, ReindexIntent::full());
    let sharded_receipt = reindex(&sharded, ReindexIntent::full());
    published_stamp(&single, &single_receipt);
    published_stamp(&sharded, &sharded_receipt);
    let single_facts = capture_public_facts(&single);
    let sharded_facts = capture_public_facts(&sharded);

    let single_status = single.status().unwrap();
    let sharded_status = sharded.status().unwrap();
    assert_ne!(
        single_status.generation_root,
        sharded_status.generation_root
    );
    assert_eq!(
        single_status.scan_roots,
        vec![fixture.assets_directory().canonicalize().unwrap()]
    );
    assert_eq!(
        sharded_status.scan_roots,
        vec![
            fixture
                .root()
                .join("Assets/Characters")
                .canonicalize()
                .unwrap(),
            fixture
                .root()
                .join("Assets/Scripts")
                .canonicalize()
                .unwrap(),
        ]
    );
    assert_eq!(single_facts.semantic(), sharded_facts.semantic());
}

#[test]
fn structured_yaml_pptrs_and_published_results_survive_source_changes() {
    let fixture = ProjectFixture::new(ProjectVersion::Final);
    let index = open_index(&fixture);
    let receipt = reindex(&index, ReindexIntent::full());
    let active = published_stamp(&index, &receipt);
    let status_before_published_queries = serde_json::to_value(index.status().unwrap()).unwrap();
    let published = capture_public_facts(&index);
    assert_eq!(
        serde_json::to_value(index.status().unwrap()).unwrap(),
        status_before_published_queries,
        "published queries must not mutate generation or runtime status"
    );

    assert_eq!(published.status.indexed_assets, 2);
    assert_eq!(published.status.indexed_search_documents, 2);
    assert_eq!(published.status.indexed_reference_facts, 5);
    assert_eq!(published.status.incomplete_assets, 0);
    assert_eq!(published.status.projection_truncations, 0);
    assert_eq!(published.hero_search.returned_hits, 1);
    assert_eq!(published.hero_search.hits[0].path, HERO_PATH);
    let mut controller_paths = published
        .controller_search
        .hits
        .iter()
        .map(|hit| hit.path.as_str())
        .collect::<Vec<_>>();
    controller_paths.sort_unstable();
    assert_eq!(controller_paths, vec![HERO_PATH, SCRIPT_PATH]);
    assert!(published.controller_search.hits.iter().any(|hit| {
        hit.matched_script_symbols
            .iter()
            .any(|symbol| symbol == "FinalController")
    }));
    assert_eq!(
        published.suggestions.suggestions,
        vec![
            "in:Assets/Characters/".to_owned(),
            "in:Assets/Scripts/".to_owned(),
        ]
    );

    assert_single_yaml_reference(
        &published.references[0],
        SCRIPT_GUID,
        -11_500_000,
        "m_Script",
    );
    assert_single_yaml_reference(&published.references[1], INLINE_GUID, -17, "m_InlineTarget");
    assert_single_yaml_reference(&published.references[2], BLOCK_GUID, -23, "m_BlockTarget");

    let scalar_false_positive = capture_references(
        &index,
        &active,
        ReferenceRequest::incoming_guid(SCRIPT_GUID, Some(-999), 20),
    );
    assert_eq!(scalar_false_positive.coverage.total, Some(0));
    assert!(scalar_false_positive.hits.is_empty());

    fs::write(
        &fixture.hero_path,
        "mutated source must not alter already published results\n",
    )
    .unwrap();
    fs::write(
        &fixture.script_path,
        "public sealed class MutatedAfterPublish {}\n",
    )
    .unwrap();
    let status_before_mutated_source_queries =
        serde_json::to_value(index.status().unwrap()).unwrap();
    assert_eq!(capture_public_facts(&index), published);
    assert_eq!(
        serde_json::to_value(index.status().unwrap()).unwrap(),
        status_before_mutated_source_queries,
        "source changes must not make queries mutate index status"
    );

    fs::remove_dir_all(fixture.assets_directory()).unwrap();
    let status_before_deleted_source_queries =
        serde_json::to_value(index.status().unwrap()).unwrap();
    assert_eq!(capture_public_facts(&index), published);
    assert_eq!(
        serde_json::to_value(index.status().unwrap()).unwrap(),
        status_before_deleted_source_queries,
        "source deletion must not make queries mutate index status"
    );
}

#[test]
fn failed_changed_path_reindex_keeps_the_prior_generation_queryable() {
    let fixture = ProjectFixture::new(ProjectVersion::Final);
    let index = open_index(&fixture);
    let receipt = reindex(&index, ReindexIntent::full());
    let before = published_stamp(&index, &receipt);
    let before_search = capture_search(&index, &before, "FinalHero");
    let before_references = capture_references(
        &index,
        &before,
        ReferenceRequest::incoming_guid(SCRIPT_GUID, Some(-11_500_000), 20),
    );
    let before_status = index.status().unwrap();

    let outside = TempDir::new().unwrap();
    let outside_path = outside.path().join("Outside.asset");
    fs::write(&outside_path, FINAL_PREFAB).unwrap();
    let mut budget = AssetLoadBudget::default();
    let error = index
        .reindex(
            ReindexIntent::changed_paths(vec![outside_path]),
            &mut budget,
        )
        .unwrap_err();

    assert_eq!(error.code(), ApiErrorCode::InvalidRequest);
    assert!(!error.retryable());
    assert_eq!(error.api_error().generation.as_ref(), Some(&before));

    let failed_status = index.status().unwrap();
    assert_eq!(active_stamp(&failed_status), before);
    assert!(!failed_status.indexing);
    let failure = failed_status
        .generation
        .last_failure
        .as_ref()
        .expect("the failed build is visible in public status");
    assert_eq!(failure.code, "invalid_request");
    assert!(!failure.retryable);
    assert_eq!(failed_status.indexed_assets, before_status.indexed_assets);
    assert_eq!(
        failed_status.indexed_search_documents,
        before_status.indexed_search_documents
    );
    assert_eq!(
        failed_status.indexed_reference_facts,
        before_status.indexed_reference_facts
    );

    assert_eq!(capture_search(&index, &before, "FinalHero"), before_search);
    assert_eq!(
        capture_references(
            &index,
            &before,
            ReferenceRequest::incoming_guid(SCRIPT_GUID, Some(-11_500_000), 20),
        ),
        before_references
    );

    let paths = index.paths().clone();
    drop(index);

    let mut reopen_budget = AssetLoadBudget::default();
    let reopened = SearchIndex::open_or_create(paths, &mut reopen_budget).unwrap();
    let reopened_status = reopened.status().unwrap();
    assert_eq!(active_stamp(&reopened_status), before);
    assert_eq!(reopened_status.indexed_assets, before_status.indexed_assets);
    assert_eq!(
        reopened_status.indexed_search_documents,
        before_status.indexed_search_documents
    );
    assert_eq!(
        reopened_status.indexed_reference_facts,
        before_status.indexed_reference_facts
    );
    assert_eq!(
        capture_search(&reopened, &before, "FinalHero"),
        before_search
    );
    assert_eq!(
        capture_references(
            &reopened,
            &before,
            ReferenceRequest::incoming_guid(SCRIPT_GUID, Some(-11_500_000), 20),
        ),
        before_references
    );
}
