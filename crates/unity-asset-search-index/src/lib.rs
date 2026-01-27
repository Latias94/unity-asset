use std::fs;
use std::io;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use ignore::{DirEntry, WalkBuilder, WalkState};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, Occur, PhrasePrefixQuery, Query, TermQuery,
};
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, Value as _};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};

use unity_asset_search_core::{
    HighlightRange, MatchKind, highlight_html, highlight_ranges, normalize_for_match, parse_query,
    rank_match, to_terms,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub guid: Option<String>,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub stable_id: String,
    pub location: Location,
    pub score: f32,
    pub match_kind: MatchKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_hierarchy_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_script_symbols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub highlight_path_ranges: Vec<HighlightRange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub highlight_name_ranges: Vec<HighlightRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight_name: Option<String>,
    #[serde(skip_serializing)]
    rank_fuzzy_score: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub took_ms: u128,
    pub total_hits: usize,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceHit {
    pub source_path: String,
    pub source_kind: String,
    pub stable_id: String,
    pub location: Location,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<ReferenceContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<ReferenceObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_file_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_class_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hierarchy_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_column: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceObject {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_file_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_class_id: Option<u32>,
    pub stable_id: String,
    pub location: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hierarchy_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencesResponse {
    pub guid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<u64>,
    pub took_ms: u128,
    pub total_hits: usize,
    pub hits: Vec<ReferenceHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestResponse {
    pub prefix: String,
    pub took_ms: u128,
    pub suggestions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexProgress {
    pub operation: String,
    pub phase: String,
    pub phase_index: u32,
    pub phase_count: u32,
    pub phases: Vec<String>,
    pub processed: u64,
    pub total: u64,
    pub has_total: bool,
    pub started_unix_ms: u64,
    pub updated_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub project_root: PathBuf,
    pub index_root_dir: PathBuf,
    pub index_data_dir: PathBuf,
    pub scan_roots: Vec<PathBuf>,
    pub ignore_files_supported: Vec<String>,
    pub project_ignore_files_present: Vec<String>,
    #[serde(default)]
    pub indexed_files: u64,
    pub indexed_docs: u64,
    pub indexed_scripts: u64,
    pub indexed_ref_sources: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_index_duration_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_index_unix_ms: Option<u64>,
    pub indexing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_scan_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_docs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_docs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reindex_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_changed_paths: Option<u64>,
    #[serde(default)]
    pub fallback_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fallback_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fallback_unix_ms: Option<u64>,
    #[serde(default)]
    pub index_bundle_container_entries: bool,
    #[serde(default)]
    pub max_bundle_container_entries_per_bundle: u64,
    #[serde(default)]
    pub respect_ignore_files: bool,
    #[serde(default)]
    pub respect_project_gitignore: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<IndexProgress>,
}

#[derive(Debug, Clone)]
pub struct IndexPaths {
    pub project_root: PathBuf,
    pub index_root_dir: PathBuf,
    pub index_data_dir: PathBuf,
    pub refs_index_data_dir: PathBuf,
    pub scan_roots: Vec<PathBuf>,
    pub state_path: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchIndexOptions {
    pub index_bundle_container_entries: bool,
    pub max_bundle_container_entries_per_bundle: usize,
    pub respect_ignore_files: bool,
    pub respect_project_gitignore: bool,
}

impl Default for SearchIndexOptions {
    fn default() -> Self {
        Self {
            index_bundle_container_entries: false,
            max_bundle_container_entries_per_bundle: 50_000,
            respect_ignore_files: true,
            respect_project_gitignore: true,
        }
    }
}

impl IndexPaths {
    pub fn for_project(
        project_root: PathBuf,
        index_root_dir: Option<PathBuf>,
        scan_roots: Option<Vec<PathBuf>>,
    ) -> Result<Self> {
        let project_root = project_root
            .canonicalize()
            .with_context(|| format!("project root does not exist: {}", project_root.display()))?;

        let index_root_dir = match index_root_dir {
            Some(p) => p,
            None => default_index_dir(&project_root),
        };

        let scan_roots = match scan_roots {
            Some(roots) if !roots.is_empty() => roots,
            _ => default_scan_roots(&project_root),
        };
        let scan_roots = normalize_scan_roots(&project_root, scan_roots)?;

        let index_data_dir = index_root_dir.join("tantivy-v2");
        let refs_index_data_dir = index_root_dir.join("refs-tantivy-v1");
        let state_path = index_root_dir.join("state-v2.json");

        Ok(Self {
            project_root,
            index_root_dir,
            index_data_dir,
            refs_index_data_dir,
            scan_roots,
            state_path,
        })
    }
}

fn default_index_dir(project_root: &Path) -> PathBuf {
    let library = project_root.join("Library");
    if library.is_dir() {
        library.join("unity-asset-search")
    } else {
        project_root.join(".unity-asset-search")
    }
}

fn default_scan_roots(project_root: &Path) -> Vec<PathBuf> {
    let assets = project_root.join("Assets");
    if !assets.is_dir() {
        return vec![project_root.to_path_buf()];
    }

    let mut roots = Vec::new();
    let candidates = [
        project_root.join("Assets"),
        project_root.join("Packages"),
        project_root.join("ProjectSettings"),
    ];
    for candidate in candidates {
        if candidate.is_dir() {
            roots.push(candidate);
        }
    }

    if roots.is_empty() {
        vec![project_root.to_path_buf()]
    } else {
        roots
    }
}

fn normalize_scan_roots(project_root: &Path, roots: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for root in roots {
        let root = if root.is_absolute() {
            root
        } else {
            project_root.join(root)
        };
        let root = root
            .canonicalize()
            .with_context(|| format!("scan root does not exist: {}", root.display()))?;
        if !root.starts_with(project_root) {
            return Err(anyhow!(
                "scan root must be inside project root: {}",
                root.display()
            ));
        }
        out.push(root);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[derive(Clone)]
pub struct SearchIndex {
    inner: Arc<RwLock<SearchIndexInner>>,
    enrich_cache: Arc<std::sync::Mutex<EnrichCache>>,
}

struct SearchIndexInner {
    options: SearchIndexOptions,
    reader: IndexReader,
    writer: IndexWriter,
    fields: SearchFields,
    refs_reader: IndexReader,
    refs_writer: IndexWriter,
    refs_fields: ReferenceFields,
    status: StatusResponse,
    progress: Option<Arc<IndexProgressState>>,
    state: IndexState,
}

#[derive(Clone)]
struct SearchFields {
    id: Field,
    guid: Field,
    path: Field,
    path_terms: Field,
    name: Field,
    name_terms: Field,
    kind: Field,
    kind_terms: Field,
    content_terms: Field,
    container_source_path: Field,
}

#[derive(Clone)]
struct ReferenceFields {
    source_id: Field,
    source_path: Field,
    source_kind: Field,
    ref_guid: Field,
    ref_guid_fileid: Field,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct Fingerprint {
    size: u64,
    mtime_ms: u64,
    #[serde(default)]
    meta_size: u64,
    #[serde(default)]
    meta_mtime_ms: u64,
}

#[derive(Debug, Clone)]
struct YamlFileCacheEntry {
    fingerprint: Fingerprint,
    is_yaml: bool,
    hierarchy_paths: Vec<String>,
    script_guids: Vec<String>,
    last_used: u64,
}

#[derive(Debug)]
struct EnrichCache {
    clock: u64,
    max_entries: usize,
    files: std::collections::HashMap<String, YamlFileCacheEntry>,
}

impl EnrichCache {
    fn new(max_entries: usize) -> Self {
        Self {
            clock: 0,
            max_entries: max_entries.max(32),
            files: std::collections::HashMap::new(),
        }
    }

    fn touch(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    fn evict_if_needed(&mut self) {
        if self.files.len() <= self.max_entries {
            return;
        }

        let mut oldest_key: Option<String> = None;
        let mut oldest_used = u64::MAX;
        for (k, v) in &self.files {
            if v.last_used < oldest_used {
                oldest_used = v.last_used;
                oldest_key = Some(k.clone());
            }
        }
        if let Some(k) = oldest_key {
            self.files.remove(&k);
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct IndexState {
    #[serde(default)]
    options: SearchIndexOptions,
    files: std::collections::BTreeMap<String, Fingerprint>,
    #[serde(default)]
    scripts: std::collections::BTreeMap<String, ScriptGuidEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScriptGuidEntry {
    rel_path: String,
    fingerprint: Fingerprint,
    terms: String,
    #[serde(default)]
    symbols: Vec<String>,
}

struct IndexingGuard {
    inner: Arc<RwLock<SearchIndexInner>>,
}

#[derive(Debug)]
struct IndexProgressState {
    operation: String,
    phases: Vec<String>,
    started_unix_ms: u64,
    updated_unix_ms: AtomicU64,
    phase_index: AtomicU32,
    processed: AtomicU64,
    has_total: AtomicBool,
    total: AtomicU64,
}

impl IndexProgressState {
    fn new(operation: impl Into<String>, phases: Vec<String>) -> Self {
        let now = unix_ms_now();
        Self {
            operation: operation.into(),
            phases,
            started_unix_ms: now,
            updated_unix_ms: AtomicU64::new(now),
            phase_index: AtomicU32::new(1),
            processed: AtomicU64::new(0),
            has_total: AtomicBool::new(false),
            total: AtomicU64::new(0),
        }
    }

    fn set_phase(&self, phase_index: u32, has_total: bool, total: u64) {
        let now = unix_ms_now();
        self.phase_index
            .store(phase_index.max(1), Ordering::Relaxed);
        self.processed.store(0, Ordering::Relaxed);
        self.has_total.store(has_total, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
        self.updated_unix_ms.store(now, Ordering::Relaxed);
    }

    fn inc_processed(&self, delta: u64) {
        if delta == 0 {
            return;
        }
        let new = self.processed.fetch_add(delta, Ordering::Relaxed) + delta;
        if new.is_multiple_of(256) {
            self.updated_unix_ms.store(unix_ms_now(), Ordering::Relaxed);
        }
    }

    fn mark_updated(&self) {
        self.updated_unix_ms.store(unix_ms_now(), Ordering::Relaxed);
    }

    fn snapshot(&self) -> IndexProgress {
        let phase_index = self.phase_index.load(Ordering::Relaxed).max(1);
        let phase = self
            .phases
            .get(phase_index.saturating_sub(1) as usize)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        IndexProgress {
            operation: self.operation.clone(),
            phase,
            phase_index,
            phase_count: self.phases.len().try_into().unwrap_or(u32::MAX),
            phases: self.phases.clone(),
            processed: self.processed.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            has_total: self.has_total.load(Ordering::Relaxed),
            started_unix_ms: self.started_unix_ms,
            updated_unix_ms: self.updated_unix_ms.load(Ordering::Relaxed),
        }
    }
}

impl Drop for IndexingGuard {
    fn drop(&mut self) {
        let Ok(mut inner) = self.inner.write() else {
            return;
        };
        if inner.status.indexing {
            inner.status.indexing = false;
        }
        inner.progress = None;
    }
}

impl SearchIndex {
    pub fn open_or_create(paths: &IndexPaths) -> Result<Self> {
        Self::open_or_create_with_options(paths, SearchIndexOptions::default())
    }

    pub fn open_or_create_with_options(
        paths: &IndexPaths,
        options: SearchIndexOptions,
    ) -> Result<Self> {
        fs::create_dir_all(&paths.index_root_dir).with_context(|| {
            format!("create index root dir: {}", paths.index_root_dir.display())
        })?;
        fs::create_dir_all(&paths.index_data_dir).with_context(|| {
            format!("create index data dir: {}", paths.index_data_dir.display())
        })?;
        fs::create_dir_all(&paths.refs_index_data_dir).with_context(|| {
            format!(
                "create refs index data dir: {}",
                paths.refs_index_data_dir.display()
            )
        })?;

        let schema = build_schema();
        let index = Index::open_in_dir(&paths.index_data_dir)
            .or_else(|_| Index::create_in_dir(&paths.index_data_dir, schema.clone()))?;

        let schema = index.schema();
        let fields = build_fields(&schema);
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        let writer = index
            .writer_with_num_threads(4, 128 * 1024 * 1024)
            .context("create index writer")?;

        let refs_schema = build_refs_schema();
        let refs_index = Index::open_in_dir(&paths.refs_index_data_dir)
            .or_else(|_| Index::create_in_dir(&paths.refs_index_data_dir, refs_schema.clone()))?;
        let refs_schema = refs_index.schema();
        let refs_fields = build_refs_fields(&refs_schema);
        let refs_reader = refs_index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let refs_writer = refs_index
            .writer_with_num_threads(2, 64 * 1024 * 1024)
            .context("create refs index writer")?;

        let state = load_state(&paths.state_path).unwrap_or_default();

        let ignore_files_supported = supported_ignore_files();
        let project_ignore_files_present =
            detect_project_ignore_files(&paths.project_root, &ignore_files_supported);

        let status = StatusResponse {
            project_root: paths.project_root.clone(),
            index_root_dir: paths.index_root_dir.clone(),
            index_data_dir: paths.index_data_dir.clone(),
            scan_roots: paths.scan_roots.clone(),
            ignore_files_supported,
            project_ignore_files_present,
            indexed_files: 0,
            indexed_docs: 0,
            indexed_scripts: 0,
            indexed_ref_sources: 0,
            last_index_duration_ms: None,
            last_index_unix_ms: None,
            indexing: false,
            last_scan_ms: None,
            updated_docs: None,
            removed_docs: None,
            last_reindex_kind: None,
            last_changed_paths: None,
            fallback_count: 0,
            last_fallback_reason: None,
            last_fallback_unix_ms: None,
            index_bundle_container_entries: options.index_bundle_container_entries,
            max_bundle_container_entries_per_bundle: options
                .max_bundle_container_entries_per_bundle
                .try_into()
                .unwrap_or(u64::MAX),
            respect_ignore_files: options.respect_ignore_files,
            respect_project_gitignore: options.respect_project_gitignore,
            progress: None,
        };

        let this = Self {
            inner: Arc::new(RwLock::new(SearchIndexInner {
                options,
                reader,
                writer,
                fields,
                refs_reader,
                refs_writer,
                refs_fields,
                status,
                progress: None,
                state,
            })),
            enrich_cache: Arc::new(std::sync::Mutex::new(EnrichCache::new(256))),
        };

        this.refresh_status()?;
        Ok(this)
    }

    pub fn status(&self) -> Result<StatusResponse> {
        self.refresh_status()?;
        let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
        let mut status = inner.status.clone();
        if let Some(progress) = inner.progress.as_ref() {
            status.progress = Some(progress.snapshot());
        }
        Ok(status)
    }

    pub fn options_changed(&self) -> bool {
        self.inner
            .read()
            .map(|inner| inner.state.options != inner.options)
            .unwrap_or(false)
    }

    pub fn note_fallback(&self, reason: &str) -> Result<()> {
        let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
        inner.status.fallback_count = inner.status.fallback_count.saturating_add(1);
        inner.status.last_fallback_reason = Some(reason.to_string());
        inner.status.last_fallback_unix_ms = Some(unix_ms_now());
        Ok(())
    }

    pub fn note_reindex_summary(&self, kind: &str, changed_paths: Option<u64>) -> Result<()> {
        let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
        inner.status.last_reindex_kind = Some(kind.to_string());
        inner.status.last_changed_paths = changed_paths;
        Ok(())
    }

    fn yaml_enrich_info_for_rel_path(
        &self,
        project_root: &Path,
        rel_path: &str,
    ) -> Option<(Vec<String>, Vec<String>)> {
        if rel_path.trim().is_empty() {
            return None;
        }

        let abs = project_root.join(rel_path);
        let fingerprint = fingerprint_for_path(&abs).ok()?;

        let mut cache = self.enrich_cache.lock().ok()?;
        let now = cache.touch();

        if let Some(entry) = cache.files.get_mut(rel_path) {
            if entry.fingerprint == fingerprint {
                entry.last_used = now;
                if entry.is_yaml {
                    return Some((entry.hierarchy_paths.clone(), entry.script_guids.clone()));
                }
                return None;
            }
        }

        let text = read_text_limited(&abs, 2 * 1024 * 1024).ok().flatten();
        let is_yaml = text.as_deref().is_some_and(is_probably_unity_yaml_text);
        let (hierarchy_paths, script_guids) = if is_yaml {
            let text = text.as_deref().unwrap_or_default();
            (
                extract_unity_yaml_hierarchy_paths(text),
                extract_unity_yaml_script_guids(text),
            )
        } else {
            (Vec::new(), Vec::new())
        };

        cache.files.insert(
            rel_path.to_string(),
            YamlFileCacheEntry {
                fingerprint,
                is_yaml,
                hierarchy_paths: hierarchy_paths.clone(),
                script_guids: script_guids.clone(),
                last_used: now,
            },
        );
        cache.evict_if_needed();

        is_yaml.then_some((hierarchy_paths, script_guids))
    }

    pub fn reindex(&self, paths: &IndexPaths) -> Result<()> {
        let options_changed = self
            .inner
            .read()
            .map(|inner| inner.state.options != inner.options)
            .unwrap_or(false);
        if options_changed {
            return self.reindex_full(paths);
        }
        self.reindex_impl(paths, ReindexMode::Incremental)
    }

    pub fn reindex_full(&self, paths: &IndexPaths) -> Result<()> {
        self.reindex_impl(paths, ReindexMode::Full)
    }

    pub fn reindex_changed_paths(
        &self,
        paths: &IndexPaths,
        changed_paths: &[PathBuf],
    ) -> Result<()> {
        let options_changed = self
            .inner
            .read()
            .map(|inner| inner.state.options != inner.options)
            .unwrap_or(false);
        if options_changed {
            return self.reindex_full(paths);
        }

        let start = Instant::now();
        let progress = Arc::new(IndexProgressState::new(
            "changed_paths",
            vec![
                "scan_changed_paths".to_string(),
                "index_documents".to_string(),
                "commit".to_string(),
                "reload".to_string(),
            ],
        ));
        let indexing_guard = {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
            inner.status.indexing = true;
            inner.status.updated_docs = None;
            inner.status.removed_docs = None;
            inner.status.last_scan_ms = None;
            progress.set_phase(1, false, 0);
            inner.progress = Some(progress.clone());
            IndexingGuard {
                inner: self.inner.clone(),
            }
        };

        let changed_paths = {
            let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
            normalize_watch_paths_for_incremental(paths, &inner.state, changed_paths)
        };
        if changed_paths.is_empty() {
            drop(indexing_guard);
            self.refresh_status()?;
            return Ok(());
        }
        progress.set_phase(1, true, changed_paths.len().try_into().unwrap_or(u64::MAX));

        let options = {
            let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
            inner.options
        };

        let scan_start = Instant::now();
        let delta = scan_changed_paths(paths, &changed_paths, options)?;
        let scan_ms = scan_start.elapsed().as_millis();
        progress.mark_updated();

        let fields = {
            let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
            inner.fields.clone()
        };
        let refs_fields = {
            let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
            inner.refs_fields.clone()
        };

        let mut updated_docs = 0u64;
        let mut removed_docs = 0u64;

        let total_work = delta
            .removed_rel_paths
            .len()
            .saturating_add(delta.files.len())
            .try_into()
            .unwrap_or(u64::MAX);
        progress.set_phase(2, true, total_work);

        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
            let mut state = inner.state.clone();
            let mut scripts = state.scripts.clone();

            for removed in &delta.removed_rel_paths {
                let removed_prefix = format!("{removed}/");
                inner
                    .writer
                    .delete_term(Term::from_field_text(fields.id, removed));
                inner
                    .writer
                    .delete_term(Term::from_field_text(fields.container_source_path, removed));
                inner
                    .refs_writer
                    .delete_term(Term::from_field_text(refs_fields.source_id, removed));
                if state.files.remove(removed).is_some() {
                    removed_docs += 1;
                }
                remove_script_entries_for_rel_path(&mut scripts, removed);
                progress.inc_processed(1);

                let removed_children: Vec<String> = state
                    .files
                    .range(removed_prefix.clone()..)
                    .take_while(|(k, _)| k.starts_with(&removed_prefix))
                    .map(|(k, _)| k.clone())
                    .collect();
                for child in removed_children {
                    inner
                        .writer
                        .delete_term(Term::from_field_text(fields.id, &child));
                    inner
                        .writer
                        .delete_term(Term::from_field_text(fields.container_source_path, &child));
                    inner
                        .refs_writer
                        .delete_term(Term::from_field_text(refs_fields.source_id, &child));
                    if state.files.remove(&child).is_some() {
                        removed_docs += 1;
                    }
                    remove_script_entries_for_rel_path(&mut scripts, &child);
                    progress.inc_processed(1);
                }
            }

            if !delta.rescan_dir_rel_prefixes.is_empty() {
                let present: std::collections::BTreeSet<String> =
                    delta.files.iter().map(|f| f.rel_path.clone()).collect();

                for prefix in &delta.rescan_dir_rel_prefixes {
                    let prefix = prefix.trim_end_matches('/');
                    if prefix.is_empty() {
                        continue;
                    }
                    let prefix_slash = format!("{prefix}/");

                    let to_remove: Vec<String> = state
                        .files
                        .range(prefix_slash.clone()..)
                        .take_while(|(k, _)| k.starts_with(&prefix_slash))
                        .filter(|(k, _)| !present.contains(*k))
                        .map(|(k, _)| k.clone())
                        .collect();

                    for rel_path in to_remove {
                        inner
                            .writer
                            .delete_term(Term::from_field_text(fields.id, &rel_path));
                        inner.writer.delete_term(Term::from_field_text(
                            fields.container_source_path,
                            &rel_path,
                        ));
                        inner
                            .refs_writer
                            .delete_term(Term::from_field_text(refs_fields.source_id, &rel_path));
                        if state.files.remove(&rel_path).is_some() {
                            removed_docs += 1;
                        }
                        remove_script_entries_for_rel_path(&mut scripts, &rel_path);
                        progress.inc_processed(1);
                    }
                }
            }

            for file in &delta.files {
                update_script_map_for_file(&mut scripts, file)?;
            }

            for file in &delta.files {
                let old = state.files.get(&file.rel_path).copied();
                if old == Some(file.fingerprint) {
                    continue;
                }

                inner
                    .writer
                    .delete_term(Term::from_field_text(fields.id, &file.rel_path));
                inner.writer.delete_term(Term::from_field_text(
                    fields.container_source_path,
                    &file.rel_path,
                ));
                inner
                    .writer
                    .add_document(build_doc(&fields, file, &scripts)?)?;

                inner
                    .refs_writer
                    .delete_term(Term::from_field_text(refs_fields.source_id, &file.rel_path));
                let (ref_doc, container_paths) =
                    build_refs_doc_and_container_entries(&refs_fields, file, options)?;
                if let Some(ref_doc) = ref_doc {
                    inner.refs_writer.add_document(ref_doc)?;
                }
                for asset_path in container_paths {
                    inner.writer.add_document(build_bundle_container_doc(
                        &fields,
                        &file.rel_path,
                        &asset_path,
                    ))?;
                }

                state.files.insert(file.rel_path.clone(), file.fingerprint);
                updated_docs += 1;
                progress.inc_processed(1);
            }

            progress.set_phase(3, false, 0);
            inner.writer.commit()?;
            inner.refs_writer.commit()?;
            progress.mark_updated();
            state.scripts = scripts;
            state.options = options;
            inner.state = state;
            store_state(&paths.state_path, &inner.state)?;
        }

        progress.set_phase(4, false, 0);
        self.inner
            .read()
            .map_err(|_| anyhow!("poisoned lock"))?
            .reader
            .reload()?;
        self.inner
            .read()
            .map_err(|_| anyhow!("poisoned lock"))?
            .refs_reader
            .reload()?;
        progress.mark_updated();

        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
            inner.status.last_index_duration_ms = Some(start.elapsed().as_millis());
            inner.status.last_scan_ms = Some(scan_ms);
            inner.status.updated_docs = Some(updated_docs);
            inner.status.removed_docs = Some(removed_docs);
            inner.status.last_reindex_kind = Some("changed_paths".to_string());
            inner.status.last_changed_paths =
                Some(changed_paths.len().try_into().unwrap_or(u64::MAX));
            inner.status.indexing = false;
            inner.status.last_index_unix_ms = Some(unix_ms_now());
            inner.progress = None;
        }

        drop(indexing_guard);
        self.refresh_status()?;
        Ok(())
    }

    fn reindex_impl(&self, paths: &IndexPaths, mode: ReindexMode) -> Result<()> {
        let start = Instant::now();
        let operation = match mode {
            ReindexMode::Incremental => "full_scan_incremental",
            ReindexMode::Full => "full_scan_full",
        };
        let progress = Arc::new(IndexProgressState::new(
            operation,
            vec![
                "scan_project_files".to_string(),
                "build_scripts".to_string(),
                "index_documents".to_string(),
                "commit".to_string(),
                "reload".to_string(),
            ],
        ));
        let indexing_guard = {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
            inner.status.indexing = true;
            inner.status.updated_docs = None;
            inner.status.removed_docs = None;
            inner.status.last_scan_ms = None;
            inner.status.last_changed_paths = None;
            let previous_files: u64 = inner.state.files.len().try_into().unwrap_or(u64::MAX);
            progress.set_phase(1, previous_files > 0, previous_files);
            inner.progress = Some(progress.clone());
            IndexingGuard {
                inner: self.inner.clone(),
            }
        };

        let options = {
            let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
            inner.options
        };

        let scan_start = Instant::now();
        let scan = scan_project_files(paths, options, Some(progress.clone()))?;
        let scan_ms = scan_start.elapsed().as_millis();
        progress.mark_updated();

        let (fields, refs_fields, mut state) = {
            let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
            (
                inner.fields.clone(),
                inner.refs_fields.clone(),
                inner.state.clone(),
            )
        };

        let script_total: u64 = scan
            .files
            .values()
            .filter(|f| f.kind == "Script")
            .count()
            .try_into()
            .unwrap_or(u64::MAX);
        progress.set_phase(2, script_total > 0, script_total);
        let scripts = build_script_guid_map(&scan, &state.scripts)?;
        if script_total > 0 {
            progress.inc_processed(script_total);
            progress.mark_updated();
        }

        let mut updated_docs = 0u64;
        let mut removed_docs = 0u64;

        let removed_rel_paths: Vec<String> = if mode == ReindexMode::Full {
            Vec::new()
        } else {
            state
                .files
                .keys()
                .filter(|path| !scan.files.contains_key(*path))
                .cloned()
                .collect()
        };
        let to_update_rel_paths: Vec<String> = if mode == ReindexMode::Full {
            scan.files.keys().cloned().collect()
        } else {
            scan.files
                .iter()
                .filter_map(|(rel_path, file)| {
                    let old = state.files.get(rel_path).copied();
                    (old != Some(file.fingerprint)).then_some(rel_path.clone())
                })
                .collect()
        };
        let total_work = removed_rel_paths
            .len()
            .saturating_add(to_update_rel_paths.len())
            .try_into()
            .unwrap_or(u64::MAX);
        progress.set_phase(3, true, total_work);

        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;

            if mode == ReindexMode::Full {
                inner.writer.delete_all_documents()?;
                inner.refs_writer.delete_all_documents()?;
                state.files.clear();
                state.scripts.clear();
            }

            for removed in &removed_rel_paths {
                inner
                    .writer
                    .delete_term(Term::from_field_text(fields.id, removed));
                inner
                    .writer
                    .delete_term(Term::from_field_text(fields.container_source_path, removed));
                inner
                    .refs_writer
                    .delete_term(Term::from_field_text(refs_fields.source_id, removed));
                state.files.remove(removed);
                removed_docs += 1;
                progress.inc_processed(1);
            }

            for rel_path in &to_update_rel_paths {
                let Some(file) = scan.files.get(rel_path) else {
                    continue;
                };

                inner
                    .writer
                    .delete_term(Term::from_field_text(fields.id, rel_path));
                inner.writer.delete_term(Term::from_field_text(
                    fields.container_source_path,
                    rel_path,
                ));
                inner
                    .writer
                    .add_document(build_doc(&fields, file, &scripts)?)?;

                inner
                    .refs_writer
                    .delete_term(Term::from_field_text(refs_fields.source_id, rel_path));
                let (ref_doc, container_paths) =
                    build_refs_doc_and_container_entries(&refs_fields, file, options)?;
                if let Some(ref_doc) = ref_doc {
                    inner.refs_writer.add_document(ref_doc)?;
                }
                for asset_path in container_paths {
                    inner.writer.add_document(build_bundle_container_doc(
                        &fields,
                        rel_path,
                        &asset_path,
                    ))?;
                }

                state.files.insert(rel_path.clone(), file.fingerprint);
                updated_docs += 1;
                progress.inc_processed(1);
            }

            progress.set_phase(4, false, 0);
            inner.writer.commit()?;
            inner.refs_writer.commit()?;
            progress.mark_updated();
            state.scripts = scripts;
            state.options = options;
            inner.state = state;
            store_state(&paths.state_path, &inner.state)?;
        }

        progress.set_phase(5, false, 0);
        self.inner
            .read()
            .map_err(|_| anyhow!("poisoned lock"))?
            .reader
            .reload()?;
        self.inner
            .read()
            .map_err(|_| anyhow!("poisoned lock"))?
            .refs_reader
            .reload()?;
        progress.mark_updated();

        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
            inner.status.last_index_duration_ms = Some(start.elapsed().as_millis());
            inner.status.last_scan_ms = Some(scan_ms);
            inner.status.updated_docs = Some(updated_docs);
            inner.status.removed_docs = Some(removed_docs);
            inner.status.last_reindex_kind = Some(
                match mode {
                    ReindexMode::Incremental => "full_scan_incremental",
                    ReindexMode::Full => "full_scan_full",
                }
                .to_string(),
            );
            inner.status.indexing = false;
            inner.status.last_index_unix_ms = Some(unix_ms_now());
            inner.progress = None;
        }

        drop(indexing_guard);
        self.refresh_status()?;
        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<SearchResponse> {
        let start = Instant::now();
        let query = query.trim();
        let spec = parse_query(query);
        if spec.raw.trim().is_empty() {
            return Ok(SearchResponse {
                query: String::new(),
                took_ms: 0,
                total_hits: 0,
                hits: Vec::new(),
            });
        }

        let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
        let searcher = inner.reader.searcher();

        let terms = to_terms(&spec.free_text);
        let tokens: Vec<&str> = terms.split_whitespace().collect();
        let mut base_query: Box<dyn Query> = build_retrieval_query(&inner.fields, &tokens);

        if let Some(kind) = spec
            .type_filter
            .as_deref()
            .and_then(canonicalize_kind_filter)
        {
            let term = Term::from_field_text(inner.fields.kind, &kind);
            let term_query = TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
            base_query = Box::new(BooleanQuery::intersection(vec![
                base_query,
                Box::new(term_query),
            ]));
        }

        let fetch_limit = if spec.type_filter.is_some() || spec.path_prefix.is_some() {
            limit * 30
        } else {
            limit * 5
        };

        let top_docs = searcher.search(&base_query, &TopDocs::with_limit(fetch_limit))?;

        let mut hits = Vec::with_capacity(top_docs.len());
        for (bm25, address) in top_docs {
            let retrieved: TantivyDocument = searcher.doc(address)?;

            let guid = retrieved
                .get_first(inner.fields.guid)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            let path = retrieved
                .get_first(inner.fields.path)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let name = retrieved
                .get_first(inner.fields.name)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let kind = retrieved
                .get_first(inner.fields.kind)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let container_source_path = retrieved
                .get_first(inner.fields.container_source_path)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            let rank_query = if spec.free_text.is_empty() {
                spec.raw.as_str()
            } else {
                spec.free_text.as_str()
            };
            let rank = rank_match(rank_query, &name, &path);
            let stable_id = if let Some(src) = container_source_path.as_deref() {
                stable_id_for(None, &format!("container:{src}|{path}"), None)
            } else {
                stable_id_for(guid.as_deref(), &path, None)
            };
            let location = Location {
                path: container_source_path.unwrap_or_else(|| path.clone()),
                guid: guid.clone(),
                file_id: None,
                class_id: None,
            };

            hits.push(SearchHit {
                guid,
                path,
                name,
                kind,
                stable_id,
                location,
                score: bm25,
                match_kind: rank.kind,
                matched_hierarchy_paths: Vec::new(),
                matched_script_symbols: Vec::new(),
                highlight_path_ranges: Vec::new(),
                highlight_name_ranges: Vec::new(),
                highlight_path: None,
                highlight_name: None,
                rank_fuzzy_score: rank.fuzzy_score,
            });
        }

        if let Some(prefix) = spec.path_prefix.as_deref() {
            let prefix_norm = normalize_for_match(prefix);
            hits.retain(|h| normalize_for_match(&h.path).starts_with(&prefix_norm));
        }

        hits.sort_by(|a, b| {
            (a.match_kind as u8, -a.rank_fuzzy_score, -a.score)
                .partial_cmp(&(b.match_kind as u8, -b.rank_fuzzy_score, -b.score))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        hits.truncate(limit);

        let tokens = spec.tokens.clone();
        for hit in &mut hits {
            hit.highlight_path_ranges = highlight_ranges(&hit.path, &tokens);
            hit.highlight_name_ranges = highlight_ranges(&hit.name, &tokens);
            hit.highlight_path = highlight_html(&hit.path, &tokens);
            hit.highlight_name = highlight_html(&hit.name, &tokens);
        }

        Ok(SearchResponse {
            query: query.to_string(),
            took_ms: start.elapsed().as_millis(),
            total_hits: hits.len(),
            hits,
        })
    }

    pub fn search_enriched(
        &self,
        project_root: &Path,
        query: &str,
        limit: usize,
    ) -> Result<SearchResponse> {
        let start = Instant::now();
        let query = query.trim();
        let spec = parse_query(query);
        if spec.raw.trim().is_empty() {
            return Ok(SearchResponse {
                query: String::new(),
                took_ms: 0,
                total_hits: 0,
                hits: Vec::new(),
            });
        }

        let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
        let searcher = inner.reader.searcher();

        let terms = to_terms(&spec.free_text);
        let tokens: Vec<&str> = terms.split_whitespace().collect();
        let mut base_query: Box<dyn Query> = build_retrieval_query(&inner.fields, &tokens);

        if let Some(kind) = spec
            .type_filter
            .as_deref()
            .and_then(canonicalize_kind_filter)
        {
            let term = Term::from_field_text(inner.fields.kind, &kind);
            let term_query = TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
            base_query = Box::new(BooleanQuery::intersection(vec![
                base_query,
                Box::new(term_query),
            ]));
        }

        let fetch_limit = if spec.type_filter.is_some() || spec.path_prefix.is_some() {
            limit * 30
        } else {
            limit * 5
        };

        let top_docs = searcher.search(&base_query, &TopDocs::with_limit(fetch_limit))?;
        drop(inner);

        let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
        let searcher = inner.reader.searcher();

        let mut hits = Vec::with_capacity(top_docs.len());
        for (bm25, address) in top_docs {
            let retrieved: TantivyDocument = searcher.doc(address)?;

            let guid = retrieved
                .get_first(inner.fields.guid)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            let path = retrieved
                .get_first(inner.fields.path)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let name = retrieved
                .get_first(inner.fields.name)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let kind = retrieved
                .get_first(inner.fields.kind)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let container_source_path = retrieved
                .get_first(inner.fields.container_source_path)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            let rank_query = if spec.free_text.is_empty() {
                spec.raw.as_str()
            } else {
                spec.free_text.as_str()
            };
            let rank = rank_match(rank_query, &name, &path);
            let stable_id = if let Some(src) = container_source_path.as_deref() {
                stable_id_for(None, &format!("container:{src}|{path}"), None)
            } else {
                stable_id_for(guid.as_deref(), &path, None)
            };
            let location = Location {
                path: container_source_path.unwrap_or_else(|| path.clone()),
                guid: guid.clone(),
                file_id: None,
                class_id: None,
            };

            hits.push(SearchHit {
                guid,
                path,
                name,
                kind,
                stable_id,
                location,
                score: bm25,
                match_kind: rank.kind,
                matched_hierarchy_paths: Vec::new(),
                matched_script_symbols: Vec::new(),
                highlight_path_ranges: Vec::new(),
                highlight_name_ranges: Vec::new(),
                highlight_path: None,
                highlight_name: None,
                rank_fuzzy_score: rank.fuzzy_score,
            });
        }

        if let Some(prefix) = spec.path_prefix.as_deref() {
            let prefix_norm = normalize_for_match(prefix);
            hits.retain(|h| normalize_for_match(&h.path).starts_with(&prefix_norm));
        }

        hits.sort_by(|a, b| {
            (a.match_kind as u8, -a.rank_fuzzy_score, -a.score)
                .partial_cmp(&(b.match_kind as u8, -b.rank_fuzzy_score, -b.score))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        hits.truncate(limit);

        enrich_hits_with_context(self, project_root, &spec, &mut hits);

        let tokens = spec.tokens.clone();
        for hit in &mut hits {
            hit.highlight_path_ranges = highlight_ranges(&hit.path, &tokens);
            hit.highlight_name_ranges = highlight_ranges(&hit.name, &tokens);
            hit.highlight_path = highlight_html(&hit.path, &tokens);
            hit.highlight_name = highlight_html(&hit.name, &tokens);
        }

        Ok(SearchResponse {
            query: query.to_string(),
            took_ms: start.elapsed().as_millis(),
            total_hits: hits.len(),
            hits,
        })
    }

    pub fn suggest(&self, prefix: &str, limit: usize) -> Result<SuggestResponse> {
        let start = Instant::now();
        let prefix = prefix.trim();
        if prefix.is_empty() {
            return Ok(SuggestResponse {
                prefix: String::new(),
                took_ms: 0,
                suggestions: Vec::new(),
            });
        }

        let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
        let mut out = Vec::new();

        let (want_kind, want_path, rest) = if let Some(rest) = prefix.strip_prefix("t:") {
            (true, false, rest)
        } else if let Some(rest) = prefix.strip_prefix("type:") {
            (true, false, rest)
        } else if let Some(rest) = prefix.strip_prefix("in:") {
            (false, true, rest)
        } else {
            (true, true, prefix)
        };

        if want_kind {
            let lower = rest.to_lowercase();
            for kind in [
                "Prefab", "Scene", "Material", "Script", "Asset", "Shader", "Texture", "Audio",
                "File",
            ] {
                if kind.to_lowercase().starts_with(&lower) {
                    out.push(format!("t:{kind}"));
                    if out.len() >= limit {
                        return Ok(SuggestResponse {
                            prefix: prefix.to_string(),
                            took_ms: start.elapsed().as_millis(),
                            suggestions: out,
                        });
                    }
                }
            }
        }

        if want_path {
            out.extend(suggest_in_paths(&inner.state, rest, limit - out.len()));
        }

        Ok(SuggestResponse {
            prefix: prefix.to_string(),
            took_ms: start.elapsed().as_millis(),
            suggestions: out,
        })
    }

    pub fn references(
        &self,
        guid: &str,
        file_id: Option<u64>,
        limit: usize,
    ) -> Result<ReferencesResponse> {
        let start = Instant::now();
        let guid = normalize_guid_string(guid.trim());
        if guid.is_empty() {
            return Ok(ReferencesResponse {
                guid: String::new(),
                file_id,
                took_ms: 0,
                total_hits: 0,
                hits: Vec::new(),
            });
        }

        let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
        let searcher = inner.refs_reader.searcher();

        let (field, term_text) = if let Some(file_id) = file_id {
            (
                inner.refs_fields.ref_guid_fileid,
                format!("{guid}:{file_id}"),
            )
        } else {
            (inner.refs_fields.ref_guid, guid.clone())
        };

        let term = Term::from_field_text(field, &term_text);
        let query = TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit.clamp(1, 500)))?;

        let mut hits = Vec::with_capacity(top_docs.len());
        for (_score, address) in top_docs {
            let retrieved: TantivyDocument = searcher.doc(address)?;
            let source_path = retrieved
                .get_first(inner.refs_fields.source_path)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let source_kind = retrieved
                .get_first(inner.refs_fields.source_kind)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let stable_id = stable_id_for(None, &source_path, None);
            hits.push(ReferenceHit {
                source_path: source_path.clone(),
                source_kind,
                stable_id,
                location: Location {
                    path: source_path.clone(),
                    guid: None,
                    file_id: None,
                    class_id: None,
                },
                contexts: Vec::new(),
                objects: Vec::new(),
            });
        }

        hits.sort_by(|a, b| {
            (a.source_path.as_str(), a.source_kind.as_str())
                .cmp(&(b.source_path.as_str(), b.source_kind.as_str()))
        });
        hits.dedup_by(|a, b| a.source_path == b.source_path);

        Ok(ReferencesResponse {
            guid,
            file_id,
            took_ms: start.elapsed().as_millis(),
            total_hits: hits.len(),
            hits,
        })
    }

    pub fn references_enriched(
        &self,
        project_root: &Path,
        guid: &str,
        file_id: Option<u64>,
        limit: usize,
    ) -> Result<ReferencesResponse> {
        let mut resp = self.references(guid, file_id, limit)?;
        let guid = resp.guid.clone();

        for hit in &mut resp.hits {
            if hit.source_path.trim().is_empty() {
                continue;
            }
            let abs = project_root.join(&hit.source_path);
            let source_guid = read_guid_from_meta(asset_meta_path(&abs))
                .map(|g| normalize_guid_string(&g))
                .filter(|g| !g.is_empty());
            hit.stable_id = stable_id_for(source_guid.as_deref(), &hit.source_path, None);
            hit.location = Location {
                path: hit.source_path.clone(),
                guid: source_guid.clone(),
                file_id: None,
                class_id: None,
            };
            let is_yaml = is_probably_unity_yaml(&abs).unwrap_or(false);
            if is_yaml {
                let Ok(Some(text)) = read_text_limited(&abs, 2 * 1024 * 1024) else {
                    continue;
                };
                hit.contexts = extract_reference_contexts_from_yaml(&text, &guid, file_id);
            } else {
                hit.contexts = extract_reference_contexts_from_binary(&abs, &guid, file_id);
            }
            let (contexts, objects) = group_reference_contexts_and_objects(
                std::mem::take(&mut hit.contexts),
                &hit.source_path,
                source_guid.as_deref(),
            );
            hit.contexts = contexts;
            hit.objects = objects;
            hit.contexts.truncate(10);
            hit.objects.truncate(10);
        }

        resp.took_ms = resp.took_ms.saturating_add(0);
        Ok(resp)
    }

    fn refresh_status(&self) -> Result<()> {
        let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
        let searcher = inner.reader.searcher();
        let refs_searcher = inner.refs_reader.searcher();

        let mut status = inner.status.clone();
        status.indexed_files = inner.state.files.len() as u64;
        status.indexed_docs = searcher.num_docs();
        status.indexed_scripts = inner.state.scripts.len() as u64;
        status.indexed_ref_sources = refs_searcher.num_docs();
        status.project_ignore_files_present =
            detect_project_ignore_files(&status.project_root, &status.ignore_files_supported);

        drop(inner);
        self.inner
            .write()
            .map_err(|_| anyhow!("poisoned lock"))?
            .status = status;

        Ok(())
    }
}

fn normalize_watch_paths_for_incremental(
    paths: &IndexPaths,
    _state: &IndexState,
    changed_paths: &[PathBuf],
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in changed_paths {
        if p.starts_with(&paths.index_root_dir) {
            continue;
        }
        if p.extension().is_some_and(|e| e == "meta") {
            let Some(asset) = asset_path_from_meta(p) else {
                continue;
            };
            out.push(asset);
            continue;
        }
        out.push(p.clone());
    }
    out.sort();
    out.dedup();
    out
}

fn supported_ignore_files() -> Vec<String> {
    vec![
        ".gitignore".to_string(),
        ".ignore".to_string(),
        ".unity-asset-search-ignore".to_string(),
    ]
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn detect_project_ignore_files(project_root: &Path, supported: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for name in supported {
        if project_root.join(name).is_file() {
            out.push(name.clone());
        }
    }
    out
}

fn stable_id_base(guid: Option<&str>, path: &str) -> String {
    if let Some(guid) = guid {
        let guid = normalize_guid_string(guid);
        if !guid.is_empty() {
            return format!("guid:{guid}");
        }
    }
    format!("path:{path}")
}

fn stable_id_for(guid: Option<&str>, path: &str, file_id: Option<u64>) -> String {
    let mut out = stable_id_base(guid, path);
    if let Some(file_id) = file_id {
        out.push('#');
        out.push_str(&file_id.to_string());
    }
    out
}

fn canonicalize_kind_filter(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let raw = raw.to_lowercase();
    let out = match raw.as_str() {
        "prefab" => "Prefab",
        "scene" => "Scene",
        "material" | "mat" => "Material",
        "script" | "cs" => "Script",
        "asset" => "Asset",
        "shader" => "Shader",
        "texture" | "tex" => "Texture",
        "audio" => "Audio",
        "bundlecontainer" | "container" | "bundle-container" => "BundleContainer",
        "file" => "File",
        _ => return None,
    };
    Some(out.to_string())
}

fn build_retrieval_query(fields: &SearchFields, tokens: &[&str]) -> Box<dyn Query> {
    if tokens.is_empty() {
        return Box::new(AllQuery);
    }

    let mut must = Vec::new();
    for token in tokens {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        must.push((Occur::Must, per_token_query(fields, token)));
    }

    if must.is_empty() {
        Box::new(AllQuery)
    } else {
        Box::new(BooleanQuery::new(must))
    }
}

fn per_token_query(fields: &SearchFields, token: &str) -> Box<dyn Query> {
    let should = vec![
        (
            Occur::Should,
            boosted_text_queries(fields.name_terms, token, 3.0, 2.0),
        ),
        (
            Occur::Should,
            boosted_text_queries(fields.path_terms, token, 2.0, 1.5),
        ),
        (
            Occur::Should,
            boosted_text_queries(fields.kind_terms, token, 1.0, 1.0),
        ),
        (
            Occur::Should,
            boosted_text_queries(fields.content_terms, token, 1.2, 1.0),
        ),
    ];

    Box::new(BooleanQuery::new(should))
}

fn boosted_text_queries(
    field: Field,
    token: &str,
    exact_boost: f32,
    prefix_boost: f32,
) -> Box<dyn Query> {
    let mut should = Vec::new();

    let term = Term::from_field_text(field, token);
    let exact = TermQuery::new(term.clone(), tantivy::schema::IndexRecordOption::Basic);
    let prefix = PhrasePrefixQuery::new(vec![term]);

    should.push((
        Occur::Should,
        Box::new(BoostQuery::new(Box::new(exact), exact_boost)) as Box<dyn Query>,
    ));
    should.push((
        Occur::Should,
        Box::new(BoostQuery::new(Box::new(prefix), prefix_boost)) as Box<dyn Query>,
    ));

    Box::new(BooleanQuery::new(should))
}

fn suggest_in_paths(state: &IndexState, raw_prefix: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }

    let mut out = std::collections::BTreeSet::new();
    let prefix = raw_prefix.trim();
    let mut scanned = 0usize;

    if prefix.is_empty() {
        for (path, _) in state.files.iter() {
            if scanned >= 2000 {
                break;
            }
            scanned += 1;
            if let Some(seg) = path.split('/').next() {
                out.insert(format!("in:{seg}/"));
            }
            if out.len() >= limit {
                break;
            }
        }
        return out.into_iter().take(limit).collect();
    }

    let start_key = prefix.to_string();
    for (path, _) in state.files.range(start_key..) {
        if scanned >= 2000 {
            break;
        }
        scanned += 1;

        if !path.starts_with(prefix) {
            break;
        }

        let suffix = &path[prefix.len()..];
        if let Some(pos) = suffix.find('/') {
            out.insert(format!("in:{}{}", prefix, &suffix[..=pos]));
        } else if let Some(pos) = path.rfind('/') {
            out.insert(format!("in:{}/", &path[..pos]));
        } else {
            out.insert(format!("in:{path}"));
        }

        if out.len() >= limit {
            break;
        }
    }

    out.into_iter().take(limit).collect()
}

fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("id", STRING | STORED);
    builder.add_text_field("guid", STRING | STORED);
    builder.add_text_field("path", STORED);
    builder.add_text_field("path_terms", TEXT);
    builder.add_text_field("name", STORED);
    builder.add_text_field("name_terms", TEXT);
    builder.add_text_field("kind", STRING | STORED);
    builder.add_text_field("kind_terms", TEXT);
    builder.add_text_field("content_terms", TEXT);
    builder.add_text_field("container_source_path", STRING | STORED);
    builder.build()
}

fn build_fields(schema: &Schema) -> SearchFields {
    SearchFields {
        id: schema.get_field("id").expect("id field"),
        guid: schema.get_field("guid").expect("guid field"),
        path: schema.get_field("path").expect("path field"),
        path_terms: schema.get_field("path_terms").expect("path_terms field"),
        name: schema.get_field("name").expect("name field"),
        name_terms: schema.get_field("name_terms").expect("name_terms field"),
        kind: schema.get_field("kind").expect("kind field"),
        kind_terms: schema.get_field("kind_terms").expect("kind_terms field"),
        content_terms: schema
            .get_field("content_terms")
            .expect("content_terms field"),
        container_source_path: schema
            .get_field("container_source_path")
            .expect("container_source_path field"),
    }
}

fn build_refs_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("source_id", STRING | STORED);
    builder.add_text_field("source_path", STORED);
    builder.add_text_field("source_kind", STRING | STORED);
    builder.add_text_field("ref_guid", STRING);
    builder.add_text_field("ref_guid_fileid", STRING);
    builder.build()
}

fn build_refs_fields(schema: &Schema) -> ReferenceFields {
    ReferenceFields {
        source_id: schema.get_field("source_id").expect("source_id"),
        source_path: schema.get_field("source_path").expect("source_path"),
        source_kind: schema.get_field("source_kind").expect("source_kind"),
        ref_guid: schema.get_field("ref_guid").expect("ref_guid"),
        ref_guid_fileid: schema
            .get_field("ref_guid_fileid")
            .expect("ref_guid_fileid"),
    }
}

#[derive(Debug, Clone)]
struct ScannedFile {
    rel_path: String,
    abs_path: PathBuf,
    fingerprint: Fingerprint,
    name: String,
    kind: String,
}

#[derive(Debug, Clone, Default)]
struct ScanResult {
    files: std::collections::BTreeMap<String, ScannedFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReindexMode {
    Incremental,
    Full,
}

fn scan_project_files(
    paths: &IndexPaths,
    options: SearchIndexOptions,
    progress: Option<Arc<IndexProgressState>>,
) -> Result<ScanResult> {
    let mut out = ScanResult::default();

    let project_root = paths.project_root.clone();
    let scan_roots = paths.scan_roots.clone();
    let files = scan_walk_parallel(
        build_project_walk_builder(paths, options),
        progress,
        move |path| {
            if path.extension().is_some_and(|e| e == "meta") {
                return Ok(None);
            }
            if should_skip_file(path)
                || is_excluded_dir(path)
                || !is_in_scan_roots_raw(&scan_roots, path)
            {
                return Ok(None);
            }

            let rel_path = path
                .strip_prefix(&project_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");

            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();

            let kind = classify_kind(path);
            let fingerprint = fingerprint_for_path(path)?;

            Ok(Some(ScannedFile {
                rel_path,
                abs_path: path.to_path_buf(),
                fingerprint,
                name,
                kind,
            }))
        },
    )?;

    for file in files {
        out.files.insert(file.rel_path.clone(), file);
    }

    Ok(out)
}

#[derive(Debug, Clone, Default)]
struct ChangeScanResult {
    files: Vec<ScannedFile>,
    removed_rel_paths: Vec<String>,
    rescan_dir_rel_prefixes: Vec<String>,
}

fn scan_changed_paths(
    paths: &IndexPaths,
    changed_paths: &[PathBuf],
    options: SearchIndexOptions,
) -> Result<ChangeScanResult> {
    let mut out = ChangeScanResult::default();
    if changed_paths.is_empty() {
        return Ok(out);
    }

    let mut candidates = Vec::new();
    for p in changed_paths {
        if p.starts_with(&paths.index_root_dir) {
            continue;
        }
        let p = if p.is_absolute() && !p.starts_with(&paths.project_root) {
            canonicalize_best_effort(p)
        } else {
            p.clone()
        };
        if p.extension().is_some_and(|e| e == "meta") {
            if let Some(asset) = asset_path_from_meta(&p) {
                candidates.push(asset);
            }
        } else {
            candidates.push(p.clone());
        }
    }

    candidates.sort();
    candidates.dedup();

    let mut existing_files = Vec::new();
    let mut rescan_dirs = Vec::new();
    for candidate in candidates {
        if !candidate.starts_with(&paths.project_root) {
            continue;
        }
        if should_skip_file(&candidate) || is_excluded_dir(&candidate) {
            continue;
        }
        if candidate.is_file() {
            existing_files.push(candidate);
        } else if candidate.exists() && candidate.is_dir() {
            if is_in_scan_roots_raw(&paths.scan_roots, &candidate) {
                rescan_dirs.push(candidate);
            }
        } else if let Ok(rel) = candidate.strip_prefix(&paths.project_root) {
            out.removed_rel_paths
                .push(rel.to_string_lossy().replace('\\', "/"));
        }
    }

    if existing_files.is_empty() && rescan_dirs.is_empty() {
        return Ok(out);
    }

    if !existing_files.is_empty() {
        let existing_set: std::collections::BTreeSet<PathBuf> =
            existing_files.into_iter().collect();
        let allowed_dirs = build_allowed_dirs(paths, &existing_set);
        let existing_set_for_filter = existing_set.clone();

        let scan_roots = paths.scan_roots.clone();
        let project_root = paths.project_root.clone();

        let mut builder = WalkBuilder::new(&project_root);
        configure_walk_builder_ignore(&mut builder, options);
        builder.filter_entry(move |e: &DirEntry| {
            let p = e.path();
            if is_excluded_dir(p) {
                return false;
            }
            if p == project_root {
                return true;
            }
            if !is_in_scan_roots_raw(&scan_roots, p) && !scan_roots.iter().any(|r| r.starts_with(p))
            {
                return false;
            }
            if e.file_type().is_some_and(|t| t.is_file()) {
                return existing_set_for_filter.contains(p);
            }
            allowed_dirs.contains(p)
        });

        let project_root = paths.project_root.clone();
        let existing_set = Arc::new(existing_set);
        let mut files = scan_walk_parallel(builder, None, move |path| {
            if should_skip_file(path) || is_excluded_dir(path) || !existing_set.contains(path) {
                return Ok(None);
            }

            let rel_path = path
                .strip_prefix(&project_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let kind = classify_kind(path);
            let fingerprint = fingerprint_for_path(path)?;

            Ok(Some(ScannedFile {
                rel_path,
                abs_path: path.to_path_buf(),
                fingerprint,
                name,
                kind,
            }))
        })?;
        out.files.append(&mut files);
    }

    for dir in normalize_rescan_dirs(rescan_dirs) {
        let Ok(rel) = dir.strip_prefix(&paths.project_root) else {
            continue;
        };
        let rel_prefix = rel
            .to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string();
        if rel_prefix.is_empty() {
            continue;
        }
        out.rescan_dir_rel_prefixes.push(rel_prefix);

        let mut files = scan_dir_files(paths, &dir, options)?;
        out.files.append(&mut files);
    }

    out.rescan_dir_rel_prefixes.sort();
    out.rescan_dir_rel_prefixes.dedup();
    out.files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    out.files.dedup_by(|a, b| a.rel_path == b.rel_path);

    Ok(out)
}

fn normalize_rescan_dirs(mut dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    dirs.sort();
    dirs.dedup();

    let mut out = Vec::new();
    for dir in dirs {
        if out.iter().any(|p: &PathBuf| dir.starts_with(p)) {
            continue;
        }
        out.retain(|p: &PathBuf| !p.starts_with(&dir));
        out.push(dir);
    }
    out
}

fn scan_dir_files(
    paths: &IndexPaths,
    dir: &Path,
    options: SearchIndexOptions,
) -> Result<Vec<ScannedFile>> {
    let project_root = paths.project_root.clone();
    let scan_roots = paths.scan_roots.clone();
    let scan_roots_for_filter = scan_roots.clone();

    let mut builder = WalkBuilder::new(dir);
    configure_walk_builder_ignore(&mut builder, options);
    builder.filter_entry(move |e: &DirEntry| {
        let p = e.path();
        if is_excluded_dir(p) {
            return false;
        }
        scan_roots_for_filter
            .iter()
            .any(|root| root.starts_with(p) || p.starts_with(root))
    });

    scan_walk_parallel(builder, None, move |path| {
        if path.extension().is_some_and(|e| e == "meta") {
            return Ok(None);
        }
        if should_skip_file(path) || is_excluded_dir(path) {
            return Ok(None);
        }
        if !is_in_scan_roots_raw(&scan_roots, path) {
            return Ok(None);
        }

        let rel_path = path
            .strip_prefix(&project_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let kind = classify_kind(path);
        let fingerprint = fingerprint_for_path(path)?;

        Ok(Some(ScannedFile {
            rel_path,
            abs_path: path.to_path_buf(),
            fingerprint,
            name,
            kind,
        }))
    })
}

fn canonicalize_best_effort(path: &Path) -> PathBuf {
    if !path.is_absolute() {
        return path.to_path_buf();
    }
    if let Ok(canon) = path.canonicalize() {
        return canon;
    }

    let mut cur = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canon) = cur.canonicalize() {
            let mut out = canon;
            for part in tail.into_iter().rev() {
                out.push(part);
            }
            return out;
        }

        let Some(name) = cur.file_name().map(|s| s.to_os_string()) else {
            break;
        };
        if !cur.pop() {
            break;
        }
        tail.push(name);
    }
    path.to_path_buf()
}

fn scan_walk_parallel<F>(
    mut builder: WalkBuilder,
    progress: Option<Arc<IndexProgressState>>,
    handle_path: F,
) -> Result<Vec<ScannedFile>>
where
    F: Fn(&Path) -> Result<Option<ScannedFile>> + Send + Sync + 'static,
{
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16);
    builder.threads(threads);

    let out = Arc::new(std::sync::Mutex::new(Vec::<ScannedFile>::new()));
    let err = Arc::new(std::sync::Mutex::new(None::<anyhow::Error>));
    let handle_path = Arc::new(handle_path);

    struct LocalCollector {
        out: Arc<std::sync::Mutex<Vec<ScannedFile>>>,
        local: Vec<ScannedFile>,
    }

    impl LocalCollector {
        fn flush(&mut self) {
            if self.local.is_empty() {
                return;
            }
            let Ok(mut out) = self.out.lock() else {
                return;
            };
            out.append(&mut self.local);
        }
    }

    impl Drop for LocalCollector {
        fn drop(&mut self) {
            self.flush();
        }
    }

    let out_for_run = out.clone();
    let err_for_run = err.clone();
    let handle_path_for_run = handle_path.clone();
    let progress_for_run = progress.clone();
    builder.build_parallel().run(|| {
        let mut collector = LocalCollector {
            out: out_for_run.clone(),
            local: Vec::new(),
        };
        let err = err_for_run.clone();
        let handle_path = handle_path_for_run.clone();
        let progress = progress_for_run.clone();
        Box::new(move |result: Result<DirEntry, ignore::Error>| {
            let Ok(entry) = result else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }

            let path = entry.path();
            if let Some(progress) = progress.as_ref() {
                progress.inc_processed(1);
            }
            match handle_path(path) {
                Ok(Some(file)) => {
                    collector.local.push(file);
                    if collector.local.len() >= 256 {
                        collector.flush();
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    if let Ok(mut err) = err.lock() {
                        if err.is_none() {
                            *err = Some(e);
                        }
                    }
                    return WalkState::Quit;
                }
            }
            WalkState::Continue
        })
    });

    if let Ok(mut err) = err.lock() {
        if let Some(e) = err.take() {
            return Err(e);
        }
    }

    let mut locked = out.lock().map_err(|_| anyhow!("poisoned lock"))?;
    let mut files = std::mem::take(&mut *locked);
    drop(locked);

    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(files)
}

fn configure_walk_builder_ignore(builder: &mut WalkBuilder, options: SearchIndexOptions) {
    builder
        .follow_links(false)
        .parents(false)
        .ignore(options.respect_ignore_files)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false);

    if options.respect_ignore_files {
        if options.respect_project_gitignore {
            builder.add_custom_ignore_filename(".gitignore");
        }
        builder.add_custom_ignore_filename(".unity-asset-search-ignore");
    }
}

fn build_project_walk_builder(paths: &IndexPaths, options: SearchIndexOptions) -> WalkBuilder {
    let scan_roots = paths.scan_roots.clone();
    let project_root = paths.project_root.clone();

    let mut builder = WalkBuilder::new(&project_root);
    configure_walk_builder_ignore(&mut builder, options);
    builder.filter_entry(move |e: &DirEntry| {
        let p = e.path();
        if is_excluded_dir(p) {
            return false;
        }
        if p == project_root {
            return true;
        }
        scan_roots
            .iter()
            .any(|root| root.starts_with(p) || p.starts_with(root))
    });

    builder
}

fn is_in_scan_roots_raw(scan_roots: &[PathBuf], path: &Path) -> bool {
    scan_roots.iter().any(|root| path.starts_with(root))
}

fn build_allowed_dirs(
    paths: &IndexPaths,
    files: &std::collections::BTreeSet<PathBuf>,
) -> std::collections::BTreeSet<PathBuf> {
    let mut out = std::collections::BTreeSet::new();
    out.insert(paths.project_root.clone());

    for file in files {
        let mut dir = file.parent();
        while let Some(d) = dir {
            out.insert(d.to_path_buf());
            if d == paths.project_root {
                break;
            }
            dir = d.parent();
        }
    }

    out
}

fn asset_path_from_meta(meta_path: &Path) -> Option<PathBuf> {
    let file_name = meta_path.file_name()?.to_str()?;
    if !file_name.ends_with(".meta") {
        return None;
    }
    let mut out = meta_path.to_path_buf();
    out.set_file_name(file_name.trim_end_matches(".meta"));
    Some(out)
}

fn build_script_guid_map(
    scan: &ScanResult,
    previous: &std::collections::BTreeMap<String, ScriptGuidEntry>,
) -> Result<std::collections::BTreeMap<String, ScriptGuidEntry>> {
    let mut out = std::collections::BTreeMap::new();

    for file in scan.files.values().filter(|f| f.kind == "Script") {
        let guid = read_guid_from_meta(asset_meta_path(&file.abs_path)).unwrap_or_default();
        if guid.trim().is_empty() {
            continue;
        }

        if let Some(prev) = previous.get(&guid) {
            if prev.fingerprint == file.fingerprint && prev.rel_path == file.rel_path {
                out.insert(guid, prev.clone());
                continue;
            }
        }

        let text = read_text_limited(&file.abs_path, 256 * 1024)?;
        let (terms, symbols) = if let Some(text) = text.as_deref() {
            (
                script_terms_for_source(file, text),
                extract_csharp_symbols(text),
            )
        } else {
            (script_terms_fallback(file), Vec::new())
        };

        out.insert(
            guid,
            ScriptGuidEntry {
                rel_path: file.rel_path.clone(),
                fingerprint: file.fingerprint,
                terms,
                symbols,
            },
        );
    }

    Ok(out)
}

fn update_script_map_for_file(
    scripts: &mut std::collections::BTreeMap<String, ScriptGuidEntry>,
    file: &ScannedFile,
) -> Result<()> {
    if file.kind != "Script" {
        return Ok(());
    }

    remove_script_entries_for_rel_path(scripts, &file.rel_path);

    let guid = read_guid_from_meta(asset_meta_path(&file.abs_path)).unwrap_or_default();
    if guid.trim().is_empty() {
        return Ok(());
    }

    if let Some(existing) = scripts.get(&guid) {
        if existing.rel_path == file.rel_path && existing.fingerprint == file.fingerprint {
            return Ok(());
        }
    }

    let text = read_text_limited(&file.abs_path, 256 * 1024)?;
    let (terms, symbols) = if let Some(text) = text.as_deref() {
        (
            script_terms_for_source(file, text),
            extract_csharp_symbols(text),
        )
    } else {
        (script_terms_fallback(file), Vec::new())
    };

    scripts.insert(
        guid,
        ScriptGuidEntry {
            rel_path: file.rel_path.clone(),
            fingerprint: file.fingerprint,
            terms,
            symbols,
        },
    );
    Ok(())
}

fn remove_script_entries_for_rel_path(
    scripts: &mut std::collections::BTreeMap<String, ScriptGuidEntry>,
    rel_path: &str,
) {
    let keys: Vec<String> = scripts
        .iter()
        .filter(|(_, entry)| entry.rel_path == rel_path)
        .map(|(guid, _)| guid.clone())
        .collect();
    for key in keys {
        scripts.remove(&key);
    }
}

fn script_terms_fallback(file: &ScannedFile) -> String {
    to_terms(&format!("{} {}", file.name, file.rel_path))
}

fn script_terms_for_source(file: &ScannedFile, text: &str) -> String {
    let symbols = extract_csharp_symbols(text);
    if symbols.is_empty() {
        return script_terms_fallback(file);
    }
    to_terms(&format!(
        "{} {} {}",
        file.name,
        symbols.join(" "),
        file.rel_path
    ))
}

fn build_doc(
    fields: &SearchFields,
    file: &ScannedFile,
    scripts: &std::collections::BTreeMap<String, ScriptGuidEntry>,
) -> Result<TantivyDocument> {
    let guid = read_guid_from_meta(asset_meta_path(&file.abs_path)).unwrap_or_default();
    let extracted = extract_content_for_file(file, scripts)?;

    let display_name = extracted
        .primary_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&file.name)
        .to_string();

    let mut document = TantivyDocument::default();
    document.add_text(fields.id, file.rel_path.clone());
    document.add_text(fields.guid, guid);
    document.add_text(fields.path, file.rel_path.clone());
    document.add_text(fields.path_terms, to_terms(&file.rel_path));
    document.add_text(fields.name, display_name.clone());
    document.add_text(fields.name_terms, to_terms(&display_name));
    document.add_text(fields.kind, file.kind.clone());
    document.add_text(fields.kind_terms, to_terms(&file.kind));

    if let Some(content_terms) = extracted.content_terms.filter(|s| !s.trim().is_empty()) {
        document.add_text(fields.content_terms, content_terms);
    }

    Ok(document)
}

fn container_name_from_asset_path(asset_path: &str) -> String {
    let asset_path = asset_path.trim();
    let file_name = asset_path
        .rsplit('/')
        .next()
        .unwrap_or(asset_path)
        .rsplit('\\')
        .next()
        .unwrap_or(asset_path)
        .trim();
    if file_name.is_empty() {
        asset_path.to_string()
    } else {
        file_name.to_string()
    }
}

fn build_bundle_container_doc(
    fields: &SearchFields,
    bundle_rel_path: &str,
    asset_path: &str,
) -> TantivyDocument {
    let asset_path = asset_path.trim();
    let bundle_rel_path = bundle_rel_path.trim();
    let display_name = container_name_from_asset_path(asset_path);

    let mut document = TantivyDocument::default();
    document.add_text(
        fields.id,
        format!("container:{bundle_rel_path}:{asset_path}"),
    );
    document.add_text(fields.guid, "");
    document.add_text(fields.path, asset_path);
    document.add_text(fields.path_terms, to_terms(asset_path));
    document.add_text(fields.name, display_name.clone());
    document.add_text(fields.name_terms, to_terms(&display_name));
    document.add_text(fields.kind, "BundleContainer");
    document.add_text(fields.kind_terms, to_terms("BundleContainer"));
    document.add_text(fields.content_terms, to_terms(bundle_rel_path));
    document.add_text(fields.container_source_path, bundle_rel_path);
    document
}

#[derive(Debug, Default, Clone)]
struct ExtractedReferences {
    guids: Vec<String>,
    guid_fileids: Vec<String>,
}

fn build_refs_doc_and_container_entries(
    fields: &ReferenceFields,
    file: &ScannedFile,
    options: SearchIndexOptions,
) -> Result<(Option<TantivyDocument>, Vec<String>)> {
    let yaml_extracted = if is_probably_unity_yaml(&file.abs_path)? {
        let text = read_text_limited(&file.abs_path, 2 * 1024 * 1024)?;
        let Some(text) = text else {
            return Ok((None, Vec::new()));
        };
        is_probably_unity_yaml_text(&text).then(|| extract_unity_yaml_references(&text))
    } else {
        None
    };

    let mut container_asset_paths: Vec<String> = Vec::new();

    let (source_kind, extracted) = if let Some(extracted) = yaml_extracted {
        (file.kind.clone(), extracted)
    } else {
        let extracted = extract_unity_binary_extraction(file, options)?;
        let Some(extracted) = extracted else {
            return Ok((None, Vec::new()));
        };
        container_asset_paths = extracted.container_asset_paths;
        (extracted.source_kind, extracted.refs)
    };

    if extracted.guids.is_empty() && extracted.guid_fileids.is_empty() {
        return Ok((None, container_asset_paths));
    }

    let mut doc = TantivyDocument::default();
    doc.add_text(fields.source_id, file.rel_path.clone());
    doc.add_text(fields.source_path, file.rel_path.clone());
    doc.add_text(fields.source_kind, source_kind);

    for guid in extracted.guids {
        doc.add_text(fields.ref_guid, guid);
    }
    for key in extracted.guid_fileids {
        doc.add_text(fields.ref_guid_fileid, key);
    }

    Ok((Some(doc), container_asset_paths))
}

#[derive(Debug, Clone)]
struct BinaryExtraction {
    source_kind: String,
    refs: ExtractedReferences,
    container_asset_paths: Vec<String>,
}

fn normalize_guid_string(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_hexdigit())
        .flat_map(|c| c.to_lowercase())
        .collect::<String>()
}

fn read_prefix(path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0u8; max_bytes];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

fn extract_unity_binary_extraction(
    file: &ScannedFile,
    options: SearchIndexOptions,
) -> Result<Option<BinaryExtraction>> {
    let prefix = read_prefix(&file.abs_path, 256).unwrap_or_default();
    let kind = unity_asset_binary::file::sniff_unity_file_kind_prefix(&prefix);
    let Some(kind) = kind else {
        return Ok(None);
    };

    let this_guid = read_guid_from_meta(asset_meta_path(&file.abs_path))
        .map(|g| normalize_guid_string(&g))
        .filter(|g| !g.is_empty());

    let unity_file = unity_asset_binary::file::load_unity_file(&file.abs_path);
    let Ok(unity_file) = unity_file else {
        return Ok(None);
    };

    let mut refs = ExtractedReferences::default();
    let mut container_asset_paths: Vec<String> = Vec::new();

    match unity_file {
        unity_asset_binary::file::UnityFile::SerializedFile(sf) => {
            refs = merge_refs(
                refs,
                extract_refs_from_serialized_file(&sf, this_guid.as_deref()),
            );
            Ok(Some(BinaryExtraction {
                source_kind: "SerializedFile".to_string(),
                refs,
                container_asset_paths,
            }))
        }
        unity_asset_binary::file::UnityFile::AssetBundle(bundle) => {
            for asset in &bundle.assets {
                refs = merge_refs(refs, extract_refs_from_serialized_file(asset, None));
                if refs.guids.len() >= 50_000 {
                    break;
                }
            }
            if options.index_bundle_container_entries
                && kind == unity_asset_binary::file::UnityFileKind::AssetBundle
            {
                let max_entries = options.max_bundle_container_entries_per_bundle.max(1);
                let mut out = std::collections::BTreeSet::<String>::new();
                for asset in &bundle.assets {
                    for info in asset.objects.iter() {
                        if info.type_id != 142 {
                            continue;
                        }
                        let Ok(entries) = asset.assetbundle_container_raw(info) else {
                            continue;
                        };
                        for (asset_path, _file_id, _path_id) in entries {
                            if asset_path.trim().is_empty() {
                                continue;
                            }
                            out.insert(asset_path);
                            if out.len() >= max_entries {
                                break;
                            }
                        }
                        if out.len() >= max_entries {
                            break;
                        }
                    }
                    if out.len() >= max_entries {
                        break;
                    }
                }
                container_asset_paths = out.into_iter().collect();
            }

            Ok(Some(BinaryExtraction {
                source_kind: "AssetBundle".to_string(),
                refs,
                container_asset_paths,
            }))
        }
        unity_asset_binary::file::UnityFile::WebFile(_) => Ok(Some(BinaryExtraction {
            source_kind: "WebFile".to_string(),
            refs,
            container_asset_paths,
        })),
    }
}

#[cfg(test)]
fn extract_assetbundle_container_asset_paths(
    bundle_path: &Path,
    max_entries: usize,
) -> Result<Vec<String>> {
    use unity_asset_binary::file::{UnityFile, UnityFileKind};

    if max_entries == 0 {
        return Ok(Vec::new());
    }

    let prefix = read_prefix(bundle_path, 256).unwrap_or_default();
    let kind = unity_asset_binary::file::sniff_unity_file_kind_prefix(&prefix);
    if kind != Some(UnityFileKind::AssetBundle) {
        return Ok(Vec::new());
    }

    let unity_file = unity_asset_binary::file::load_unity_file(bundle_path);
    let Ok(unity_file) = unity_file else {
        return Ok(Vec::new());
    };

    let UnityFile::AssetBundle(bundle) = unity_file else {
        return Ok(Vec::new());
    };

    let mut out = std::collections::BTreeSet::<String>::new();
    for asset in &bundle.assets {
        for info in asset.objects.iter() {
            if info.type_id != 142 {
                continue;
            }
            let Ok(entries) = asset.assetbundle_container_raw(info) else {
                continue;
            };
            for (asset_path, _file_id, _path_id) in entries {
                if asset_path.trim().is_empty() {
                    continue;
                }
                out.insert(asset_path);
                if out.len() >= max_entries {
                    return Ok(out.into_iter().collect());
                }
            }
        }
    }

    Ok(out.into_iter().collect())
}

fn merge_refs(mut a: ExtractedReferences, b: ExtractedReferences) -> ExtractedReferences {
    a.guids.extend(b.guids);
    a.guid_fileids.extend(b.guid_fileids);
    a.guids.sort();
    a.guids.dedup();
    a.guid_fileids.sort();
    a.guid_fileids.dedup();
    a
}

fn extract_refs_from_serialized_file(
    file: &unity_asset_binary::asset::SerializedFile,
    self_guid: Option<&str>,
) -> ExtractedReferences {
    const MAX_OBJECTS: usize = 20_000;

    let mut guids = std::collections::BTreeSet::<String>::new();
    let mut guid_fileids = std::collections::BTreeSet::<String>::new();

    let externals: Vec<Option<String>> = file
        .externals
        .iter()
        .map(|e| {
            let g = normalize_guid_string(&e.guid_string());
            (!g.is_empty()).then_some(g)
        })
        .collect();

    for info in file.objects.iter().take(MAX_OBJECTS) {
        let handle = unity_asset_binary::object::ObjectHandle::new(file, info);
        let Ok(Some(pptrs)) = handle.scan_pptrs() else {
            continue;
        };

        if let Some(self_guid) = self_guid {
            for id in pptrs.internal {
                if id > 0 {
                    let g = self_guid.to_string();
                    guids.insert(g.clone());
                    guid_fileids.insert(format!("{g}:{id}"));
                }
            }
        }

        for (file_id, path_id) in pptrs.external {
            if path_id <= 0 {
                continue;
            }
            if file_id <= 0 {
                continue;
            }
            let idx: usize = (file_id - 1).try_into().unwrap_or(usize::MAX);
            let Some(Some(guid)) = externals.get(idx) else {
                continue;
            };
            guids.insert(guid.clone());
            guid_fileids.insert(format!("{guid}:{path_id}"));
        }

        if guids.len() >= 50_000 {
            break;
        }
    }

    ExtractedReferences {
        guids: guids.into_iter().collect(),
        guid_fileids: guid_fileids.into_iter().collect(),
    }
}

fn extract_unity_yaml_references(text: &str) -> ExtractedReferences {
    static PPTR_GUID_FIRST: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"\{[^}]*\bguid:\s*([0-9a-fA-F]{32})\b[^}]*\bfileID:\s*([0-9]+)\b[^}]*\}")
            .expect("pptr guid-first regex")
    });
    static PPTR_FILEID_FIRST: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"\{[^}]*\bfileID:\s*([0-9]+)\b[^}]*\bguid:\s*([0-9a-fA-F]{32})\b[^}]*\}")
            .expect("pptr fileID-first regex")
    });
    static GUID_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"\bguid:\s*([0-9a-fA-F]{32})\b").expect("guid regex")
    });

    let mut guids = std::collections::BTreeSet::<String>::new();
    let mut guid_fileids = std::collections::BTreeSet::<String>::new();

    for cap in PPTR_GUID_FIRST.captures_iter(text).take(20_000) {
        let Some(guid) = cap.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let Some(file_id) = cap.get(2).map(|m| m.as_str()) else {
            continue;
        };
        let guid = guid.to_lowercase();
        guids.insert(guid.clone());
        guid_fileids.insert(format!("{guid}:{}", file_id));
        if guids.len() >= 50_000 {
            break;
        }
    }

    for cap in PPTR_FILEID_FIRST.captures_iter(text).take(20_000) {
        let Some(file_id) = cap.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let Some(guid) = cap.get(2).map(|m| m.as_str()) else {
            continue;
        };
        let guid = guid.to_lowercase();
        guids.insert(guid.clone());
        guid_fileids.insert(format!("{guid}:{}", file_id));
        if guids.len() >= 50_000 {
            break;
        }
    }

    for cap in GUID_RE.captures_iter(text).take(50_000) {
        let Some(guid) = cap.get(1).map(|m| m.as_str()) else {
            continue;
        };
        guids.insert(guid.to_lowercase());
        if guids.len() >= 50_000 {
            break;
        }
    }

    ExtractedReferences {
        guids: guids.into_iter().collect(),
        guid_fileids: guid_fileids.into_iter().collect(),
    }
}

#[derive(Debug, Default)]
struct ExtractedContent {
    primary_name: Option<String>,
    content_terms: Option<String>,
}

fn extract_content_for_file(
    file: &ScannedFile,
    scripts: &std::collections::BTreeMap<String, ScriptGuidEntry>,
) -> Result<ExtractedContent> {
    let ext = file
        .abs_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if matches!(
        file.kind.as_str(),
        "Prefab" | "Scene" | "Material" | "Asset"
    ) && is_probably_unity_yaml(&file.abs_path)?
    {
        let text = read_text_limited(&file.abs_path, 2 * 1024 * 1024)?;
        let Some(text) = text else {
            return Ok(ExtractedContent::default());
        };
        return Ok(extract_unity_yaml_content(&text, scripts));
    }

    if matches!(
        ext.as_str(),
        "cs" | "shader"
            | "cginc"
            | "hlsl"
            | "compute"
            | "json"
            | "asmdef"
            | "asmref"
            | "uxml"
            | "uss"
            | "txt"
            | "md"
            | "yaml"
            | "yml"
    ) {
        let text = read_text_limited(&file.abs_path, 256 * 1024)?;
        let Some(text) = text else {
            return Ok(ExtractedContent::default());
        };

        let mut combined = String::new();
        if ext == "cs" {
            let csharp_terms = extract_csharp_terms(&text);
            if !csharp_terms.is_empty() {
                combined.push_str(&csharp_terms);
                combined.push(' ');
            }
        }
        combined.push_str(&text);

        return Ok(ExtractedContent {
            primary_name: None,
            content_terms: Some(to_terms(&combined)),
        });
    }

    Ok(ExtractedContent::default())
}

#[derive(Debug, Clone, Copy)]
struct DocHeader {
    class_id: u32,
    file_id: u64,
}

#[derive(Debug, Clone)]
struct YamlDocInfo {
    name: Option<String>,
    game_object_id: Option<u64>,
}

fn extract_reference_contexts_from_yaml(
    text: &str,
    guid: &str,
    file_id: Option<u64>,
) -> Vec<ReferenceContext> {
    let guid = guid.trim().to_lowercase();
    if guid.is_empty() {
        return Vec::new();
    }

    let analysis = analyze_unity_yaml_docs(text);
    let mut out = Vec::new();

    let guid_needle = guid.as_str();
    let fileid_needle = file_id.map(|id| id.to_string());
    let mut current: Option<DocHeader> = None;

    for (line_idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim_end();
        if let Some((class_id, doc_file_id)) = parse_unity_yaml_doc_header(line) {
            current = Some(DocHeader {
                class_id,
                file_id: doc_file_id,
            });
            continue;
        }

        let Some(header) = current else {
            continue;
        };

        if !line.contains(guid_needle) {
            continue;
        }
        if let Some(fid) = fileid_needle.as_deref() {
            if !line.contains(fid) {
                continue;
            }
        }

        let field_hint = guess_field_hint(line);
        let (object_name, hierarchy_path) =
            analysis.context_for_doc(header.file_id).unwrap_or_default();

        let source_line = Some((line_idx.saturating_add(1)).try_into().unwrap_or(u32::MAX));
        let source_column = raw_line
            .find(guid_needle)
            .map(|idx| (idx.saturating_add(1)).try_into().unwrap_or(u32::MAX));

        out.push(ReferenceContext {
            doc_file_id: Some(header.file_id),
            doc_class_id: Some(header.class_id),
            object_name,
            hierarchy_path,
            field_hint,
            source_line,
            source_column,
        });

        if out.len() >= 20 {
            break;
        }
    }

    out.sort_by(|a, b| {
        (
            a.doc_file_id.unwrap_or(0),
            a.doc_class_id.unwrap_or(0),
            a.hierarchy_path.as_deref().unwrap_or(""),
            a.object_name.as_deref().unwrap_or(""),
            a.field_hint.as_deref().unwrap_or(""),
            a.source_line.unwrap_or(0),
        )
            .cmp(&(
                b.doc_file_id.unwrap_or(0),
                b.doc_class_id.unwrap_or(0),
                b.hierarchy_path.as_deref().unwrap_or(""),
                b.object_name.as_deref().unwrap_or(""),
                b.field_hint.as_deref().unwrap_or(""),
                b.source_line.unwrap_or(0),
            ))
    });
    out.dedup_by(|a, b| {
        if a.doc_file_id == b.doc_file_id
            && a.doc_class_id == b.doc_class_id
            && a.object_name == b.object_name
            && a.hierarchy_path == b.hierarchy_path
            && a.field_hint == b.field_hint
        {
            if let Some(b_line) = b.source_line {
                let take = match a.source_line {
                    None => true,
                    Some(a_line) => b_line < a_line,
                };
                if take {
                    a.source_line = b.source_line;
                    a.source_column = b.source_column;
                }
            }
            return true;
        }
        a.doc_file_id == b.doc_file_id
            && a.doc_class_id == b.doc_class_id
            && a.object_name == b.object_name
            && a.hierarchy_path == b.hierarchy_path
            && a.field_hint == b.field_hint
    });
    out.truncate(20);
    out
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ContextKey {
    doc_file_id: Option<u64>,
    doc_class_id: Option<u32>,
    object_name: Option<String>,
    hierarchy_path: Option<String>,
}

fn group_reference_contexts_and_objects(
    contexts: Vec<ReferenceContext>,
    source_path: &str,
    source_guid: Option<&str>,
) -> (Vec<ReferenceContext>, Vec<ReferenceObject>) {
    if contexts.is_empty() {
        return (contexts, Vec::new());
    }

    let mut grouped: std::collections::BTreeMap<
        ContextKey,
        (ReferenceContext, std::collections::BTreeSet<String>),
    > = std::collections::BTreeMap::new();

    for ctx in contexts {
        let key = ContextKey {
            doc_file_id: ctx.doc_file_id,
            doc_class_id: ctx.doc_class_id,
            object_name: ctx.object_name.clone(),
            hierarchy_path: ctx.hierarchy_path.clone(),
        };

        let entry = grouped.entry(key).or_insert_with(|| {
            let mut base = ctx.clone();
            base.field_hint = None;
            (base, std::collections::BTreeSet::new())
        });

        if let Some(ctx_line) = ctx.source_line {
            let take = match entry.0.source_line {
                None => true,
                Some(existing_line) => ctx_line < existing_line,
            };
            if take {
                entry.0.source_line = Some(ctx_line);
                entry.0.source_column = ctx.source_column;
            }
        }

        if let Some(hint) = ctx.field_hint {
            if !hint.trim().is_empty() {
                entry.1.insert(hint);
            }
        }
    }

    let mut contexts = Vec::with_capacity(grouped.len());
    let mut objects = Vec::with_capacity(grouped.len());

    for (mut base, hints) in grouped.into_values() {
        let field_hints: Vec<String> = hints.iter().cloned().collect();
        let file_id = base.doc_file_id;
        objects.push(ReferenceObject {
            doc_file_id: base.doc_file_id,
            doc_class_id: base.doc_class_id,
            stable_id: stable_id_for(source_guid, source_path, file_id),
            location: Location {
                path: source_path.to_string(),
                guid: source_guid.map(str::to_string),
                file_id,
                class_id: base.doc_class_id,
            },
            object_name: base.object_name.clone(),
            hierarchy_path: base.hierarchy_path.clone(),
            field_hints,
        });
        base.field_hint = join_hints(hints);
        contexts.push(base);
    }

    (contexts, objects)
}

fn join_hints(hints: std::collections::BTreeSet<String>) -> Option<String> {
    let mut out = String::new();
    for hint in hints {
        if out.is_empty() {
            out.push_str(&hint);
        } else {
            if out.len() >= 120 {
                out.push_str(", …");
                break;
            }
            out.push_str(", ");
            out.push_str(&hint);
        }
    }
    (!out.is_empty()).then_some(out)
}

fn extract_reference_contexts_from_binary(
    abs_path: &Path,
    guid: &str,
    file_id: Option<u64>,
) -> Vec<ReferenceContext> {
    let guid = normalize_guid_string(guid);
    if guid.is_empty() {
        return Vec::new();
    }

    let prefix = read_prefix(abs_path, 256).unwrap_or_default();
    let kind = unity_asset_binary::file::sniff_unity_file_kind_prefix(&prefix);
    if kind.is_none() {
        return Vec::new();
    }

    let self_guid = read_guid_from_meta(asset_meta_path(abs_path))
        .map(|g| normalize_guid_string(&g))
        .filter(|g| !g.is_empty());

    let unity_file = unity_asset_binary::file::load_unity_file(abs_path);
    let Ok(unity_file) = unity_file else {
        return Vec::new();
    };

    match unity_file {
        unity_asset_binary::file::UnityFile::SerializedFile(sf) => {
            extract_reference_contexts_from_serialized_file(
                &sf,
                self_guid.as_deref(),
                &guid,
                file_id,
            )
        }
        unity_asset_binary::file::UnityFile::AssetBundle(bundle) => {
            let mut out = Vec::new();
            for (idx, asset) in bundle.assets.iter().enumerate() {
                let asset_name = bundle.asset_names.get(idx).cloned().unwrap_or_default();
                let mut ctx =
                    extract_reference_contexts_from_serialized_file(asset, None, &guid, file_id);
                if !asset_name.trim().is_empty() {
                    for c in &mut ctx {
                        let hint = c.field_hint.clone().unwrap_or_else(|| "PPtr".to_string());
                        c.field_hint = Some(format!("bundle_asset={asset_name} {hint}"));
                    }
                }
                out.extend(ctx);
                if out.len() >= 20 {
                    break;
                }
            }
            out.truncate(20);
            out
        }
        unity_asset_binary::file::UnityFile::WebFile(_) => Vec::new(),
    }
}

fn extract_reference_contexts_from_serialized_file(
    file: &unity_asset_binary::asset::SerializedFile,
    self_guid: Option<&str>,
    target_guid: &str,
    target_file_id: Option<u64>,
) -> Vec<ReferenceContext> {
    const MAX_OBJECTS: usize = 50_000;
    const MAX_CONTEXTS: usize = 20;

    let externals: Vec<Option<String>> = file
        .externals
        .iter()
        .map(|e| {
            let g = normalize_guid_string(&e.guid_string());
            (!g.is_empty()).then_some(g)
        })
        .collect();

    let mut out = Vec::new();
    for info in file.objects.iter().take(MAX_OBJECTS) {
        if out.len() >= MAX_CONTEXTS {
            break;
        }

        let handle = unity_asset_binary::object::ObjectHandle::new(file, info);
        let Ok(Some(pptrs)) = handle.scan_pptrs() else {
            continue;
        };

        let mut matched = false;
        let mut field_hint = None;

        if let Some(self_guid) = self_guid
            && self_guid == target_guid
        {
            for id in &pptrs.internal {
                let Ok(path_id_u64) = u64::try_from(*id) else {
                    continue;
                };
                if target_file_id.is_some_and(|want| want != path_id_u64) {
                    continue;
                }
                matched = true;
                field_hint = Some(format!("binary internal pathID={path_id_u64}"));
                break;
            }
        }

        if !matched {
            for (file_id, path_id) in &pptrs.external {
                let Ok(path_id_u64) = u64::try_from(*path_id) else {
                    continue;
                };
                if target_file_id.is_some_and(|want| want != path_id_u64) {
                    continue;
                }
                if *file_id <= 0 {
                    continue;
                }
                let idx: usize = (*file_id - 1).try_into().unwrap_or(usize::MAX);
                let Some(Some(guid)) = externals.get(idx) else {
                    continue;
                };
                if guid != target_guid {
                    continue;
                }
                matched = true;
                field_hint = Some(format!(
                    "binary external fileID={file_id} pathID={path_id_u64}"
                ));
                break;
            }
        }

        if !matched {
            continue;
        }

        let doc_file_id = u64::try_from(handle.path_id()).ok();
        let doc_class_id = u32::try_from(handle.class_id()).ok();
        let object_name = handle.peek_name().ok().flatten();

        out.push(ReferenceContext {
            doc_file_id,
            doc_class_id,
            object_name,
            hierarchy_path: None,
            field_hint,
            source_line: None,
            source_column: None,
        });
    }

    out
}

fn guess_field_hint(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if let Some(pos) = trimmed.find(':') {
        let key = trimmed[..pos].trim();
        if !key.is_empty()
            && key.len() <= 64
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Some(key.to_string());
        }
    }
    if trimmed.contains("m_Script:") {
        return Some("m_Script".to_string());
    }
    None
}

#[derive(Debug, Default)]
struct YamlAnalysis {
    docs: std::collections::BTreeMap<u64, YamlDocInfo>,
    go_names: std::collections::BTreeMap<u64, String>,
    transforms: std::collections::BTreeMap<u64, TransformLink>,
    transform_by_game_object: std::collections::BTreeMap<u64, u64>,
    hierarchy_path_by_transform: std::collections::BTreeMap<u64, String>,
}

impl YamlAnalysis {
    fn context_for_doc(&self, doc_file_id: u64) -> Option<(Option<String>, Option<String>)> {
        let doc = self.docs.get(&doc_file_id)?;
        let object_name = doc.name.clone().or_else(|| {
            doc.game_object_id
                .and_then(|go| self.go_names.get(&go).cloned())
        });

        let hierarchy_path = doc
            .game_object_id
            .and_then(|go| self.transform_by_game_object.get(&go).copied())
            .and_then(|t| self.hierarchy_path_by_transform.get(&t).cloned());

        Some((object_name, hierarchy_path))
    }
}

fn analyze_unity_yaml_docs(text: &str) -> YamlAnalysis {
    let mut analysis = YamlAnalysis::default();

    let mut current: Option<DocHeader> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim_end();

        if let Some((class_id, file_id)) = parse_unity_yaml_doc_header(line) {
            current = Some(DocHeader { class_id, file_id });
            analysis.docs.entry(file_id).or_insert_with(|| YamlDocInfo {
                name: None,
                game_object_id: None,
            });
            continue;
        }

        let Some(header) = current else {
            continue;
        };

        match header.class_id {
            1 => {
                if let Some(name) = parse_unity_yaml_scalar(line, "m_Name") {
                    if !name.trim().is_empty() {
                        analysis
                            .go_names
                            .entry(header.file_id)
                            .or_insert(name.clone());
                        analysis.docs.entry(header.file_id).and_modify(|d| {
                            d.name.get_or_insert(name);
                        });
                    }
                }
            }
            4 | 224 => {
                let entry = analysis
                    .transforms
                    .entry(header.file_id)
                    .or_insert(TransformLink {
                        game_object_id: None,
                        father_transform_id: None,
                    });

                if entry.game_object_id.is_none() && line.contains("m_GameObject:") {
                    entry.game_object_id = parse_file_id(line);
                }
                if entry.father_transform_id.is_none() && line.contains("m_Father:") {
                    entry.father_transform_id = parse_file_id(line).filter(|id| *id != 0);
                }
            }
            _ => {
                let doc = analysis
                    .docs
                    .entry(header.file_id)
                    .or_insert_with(|| YamlDocInfo {
                        name: None,
                        game_object_id: None,
                    });
                if doc.name.is_none() {
                    if let Some(name) = parse_unity_yaml_scalar(line, "m_Name") {
                        if !name.trim().is_empty() {
                            doc.name = Some(name);
                        }
                    }
                }
                if doc.game_object_id.is_none() && line.contains("m_GameObject:") {
                    doc.game_object_id = parse_file_id(line);
                }
            }
        }
    }

    for (transform_id, link) in &analysis.transforms {
        if let Some(go) = link.game_object_id {
            analysis.transform_by_game_object.insert(go, *transform_id);
        }
    }

    let mut cache = std::collections::BTreeMap::<u64, Option<String>>::new();
    for (transform_id, link) in &analysis.transforms {
        let Some(go_id) = link.game_object_id else {
            continue;
        };
        let Some(leaf_name) = analysis.go_names.get(&go_id) else {
            continue;
        };
        if let Some(path) = resolve_transform_path(
            *transform_id,
            leaf_name,
            &analysis.go_names,
            &analysis.transforms,
            &mut cache,
        ) {
            analysis
                .hierarchy_path_by_transform
                .insert(*transform_id, path);
        }
    }

    analysis
}

fn read_text_limited(path: &Path, max_bytes: usize) -> Result<Option<String>> {
    let file = fs::File::open(path)?;
    let mut buf = Vec::new();
    file.take(max_bytes as u64).read_to_end(&mut buf)?;
    if buf.contains(&0) {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&buf).to_string()))
}

fn is_probably_unity_yaml(path: &Path) -> Result<bool> {
    let mut file = fs::File::open(path)?;
    let mut buf = [0u8; 256];
    let n = file.read(&mut buf)?;
    let head = &buf[..n];
    if head.contains(&0) {
        return Ok(false);
    }
    let head = String::from_utf8_lossy(head);
    Ok(head.contains("%YAML") || head.contains("!u!") || head.contains("---"))
}

fn extract_unity_yaml_content(
    text: &str,
    scripts: &std::collections::BTreeMap<String, ScriptGuidEntry>,
) -> ExtractedContent {
    static NAME_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"(?m)^\s*m_Name:\s*(.+?)\s*$").expect("m_Name regex")
    });
    static TAG_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"(?m)^\s*m_TagString:\s*(.+?)\s*$").expect("m_TagString regex")
    });
    static GUID_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"\bguid:\s*([0-9a-fA-F]{32})\b").expect("guid ref regex")
    });
    static FILEID_RE: once_cell::sync::Lazy<Regex> =
        once_cell::sync::Lazy::new(|| Regex::new(r"\bfileID:\s*([0-9]+)\b").expect("fileID regex"));

    let mut primary_name = None;
    let mut extracted: Vec<String> = Vec::new();
    let mut referenced_guids: Vec<String> = Vec::new();

    for cap in NAME_RE.captures_iter(text).take(512) {
        let Some(raw) = cap.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let value = raw.trim().trim_matches('"').trim();
        if value.is_empty() {
            continue;
        }
        if primary_name.is_none() {
            primary_name = Some(value.to_string());
        }
        extracted.push(value.to_string());
        if extracted.len() >= 2048 {
            break;
        }
    }

    for path in extract_unity_yaml_hierarchy_paths(text)
        .into_iter()
        .take(512)
    {
        if extracted.len() >= 4096 {
            break;
        }
        extracted.push(path);
    }

    for cap in TAG_RE.captures_iter(text).take(256) {
        let Some(raw) = cap.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let value = raw.trim().trim_matches('"').trim();
        if value.is_empty() {
            continue;
        }
        extracted.push(value.to_string());
        if extracted.len() >= 2048 {
            break;
        }
    }

    for cap in GUID_RE.captures_iter(text).take(1024) {
        let Some(guid) = cap.get(1).map(|m| m.as_str()) else {
            continue;
        };
        extracted.push(guid.to_string());
        referenced_guids.push(guid.to_string());
        if extracted.len() >= 4096 {
            break;
        }
    }

    for cap in FILEID_RE.captures_iter(text).take(1024) {
        let Some(id) = cap.get(1).map(|m| m.as_str()) else {
            continue;
        };
        extracted.push(id.to_string());
        if extracted.len() >= 4096 {
            break;
        }
    }

    referenced_guids.sort();
    referenced_guids.dedup();

    let base_terms = if extracted.is_empty() {
        None
    } else {
        Some(to_terms(&extracted.join(" ")))
    };

    let mut resolved = String::new();
    for guid in referenced_guids {
        let Some(entry) = scripts.get(&guid) else {
            continue;
        };
        resolved.push_str(&entry.terms);
        resolved.push(' ');
    }
    let resolved = resolved.trim();

    let content_terms = match (base_terms, resolved.is_empty()) {
        (None, _) => None,
        (Some(base), true) => Some(base),
        (Some(base), false) => Some(format!("{base} {resolved}").trim().to_string()),
    };

    ExtractedContent {
        primary_name,
        content_terms,
    }
}

#[derive(Debug, Clone, Copy)]
struct TransformLink {
    game_object_id: Option<u64>,
    father_transform_id: Option<u64>,
}

fn extract_unity_yaml_hierarchy_paths(text: &str) -> Vec<String> {
    let mut go_names: std::collections::BTreeMap<u64, String> = std::collections::BTreeMap::new();
    let mut transforms: std::collections::BTreeMap<u64, TransformLink> =
        std::collections::BTreeMap::new();

    let mut current_class: Option<u32> = None;
    let mut current_id: Option<u64> = None;

    for line in text.lines() {
        let line = line.trim_end();

        if let Some((class_id, file_id)) = parse_unity_yaml_doc_header(line) {
            current_class = Some(class_id);
            current_id = Some(file_id);
            continue;
        }

        let Some(class_id) = current_class else {
            continue;
        };
        let Some(file_id) = current_id else {
            continue;
        };

        match class_id {
            1 => {
                if let Some(name) = parse_unity_yaml_scalar(line, "m_Name") {
                    if !name.trim().is_empty() {
                        go_names.entry(file_id).or_insert(name);
                    }
                }
            }
            4 | 224 => {
                let entry = transforms.entry(file_id).or_insert(TransformLink {
                    game_object_id: None,
                    father_transform_id: None,
                });

                if entry.game_object_id.is_none() && line.contains("m_GameObject:") {
                    entry.game_object_id = parse_file_id(line);
                }
                if entry.father_transform_id.is_none() && line.contains("m_Father:") {
                    let father = parse_file_id(line).filter(|id| *id != 0);
                    entry.father_transform_id = father;
                }
            }
            _ => {}
        }
    }

    let mut out = std::collections::BTreeSet::<String>::new();
    let mut cached_paths: std::collections::BTreeMap<u64, Option<String>> =
        std::collections::BTreeMap::new();

    for (transform_id, link) in &transforms {
        let Some(go_id) = link.game_object_id else {
            continue;
        };
        let Some(leaf_name) = go_names.get(&go_id) else {
            continue;
        };
        let Some(path) = resolve_transform_path(
            *transform_id,
            leaf_name,
            &go_names,
            &transforms,
            &mut cached_paths,
        ) else {
            continue;
        };
        out.insert(path);
        if out.len() >= 256 {
            break;
        }
    }

    out.into_iter().collect()
}

fn resolve_transform_path(
    transform_id: u64,
    leaf_name: &str,
    go_names: &std::collections::BTreeMap<u64, String>,
    transforms: &std::collections::BTreeMap<u64, TransformLink>,
    cache: &mut std::collections::BTreeMap<u64, Option<String>>,
) -> Option<String> {
    if let Some(cached) = cache.get(&transform_id) {
        return cached.clone();
    }

    let mut seen = std::collections::BTreeSet::new();
    let mut parts: Vec<String> = Vec::new();
    parts.push(leaf_name.to_string());

    let mut current = transforms
        .get(&transform_id)
        .and_then(|t| t.father_transform_id);
    while let Some(parent_id) = current {
        if !seen.insert(parent_id) {
            break;
        }
        let Some(parent) = transforms.get(&parent_id) else {
            break;
        };
        let Some(parent_go) = parent.game_object_id else {
            break;
        };
        let Some(parent_name) = go_names.get(&parent_go) else {
            break;
        };
        parts.push(parent_name.to_string());
        current = parent.father_transform_id;
        if parts.len() >= 32 {
            break;
        }
    }

    parts.reverse();
    let path = parts.join("/");
    let out = Some(path);
    cache.insert(transform_id, out.clone());
    out
}

fn parse_unity_yaml_doc_header(line: &str) -> Option<(u32, u64)> {
    // Example: --- !u!1 &123456
    let line = line.trim();
    if !line.starts_with("---") {
        return None;
    }
    let u_pos = line.find("!u!")?;
    let after_u = &line[u_pos + 3..];
    let class_part = after_u.split_whitespace().next()?;
    let class_str = class_part.trim_start_matches('!');
    let class_id: u32 = class_str.parse().ok()?;

    let amp_pos = line.rfind('&')?;
    let id_str = line[amp_pos + 1..].trim();
    let file_id: u64 = id_str.parse().ok()?;
    Some((class_id, file_id))
}

fn parse_unity_yaml_scalar(line: &str, key: &str) -> Option<String> {
    // m_Name: Foo
    let trimmed = line.trim_start();
    let prefix = format!("{key}:");
    if !trimmed.starts_with(&prefix) {
        return None;
    }
    let value = trimmed[prefix.len()..].trim();
    Some(value.trim_matches('"').to_string())
}

fn parse_file_id(line: &str) -> Option<u64> {
    // {fileID: 123} or fileID: 123
    let idx = line.find("fileID:")?;
    let after = &line[idx + "fileID:".len()..];
    let digits = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    digits.parse().ok()
}

fn extract_csharp_terms(text: &str) -> String {
    extract_csharp_symbols(text).join(" ")
}

fn extract_csharp_symbols(text: &str) -> Vec<String> {
    static TYPE_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(
            r"(?m)^\s*(?:\[[^\]]+\]\s*)*(?:public|private|protected|internal|static|sealed|partial|abstract|new|\s)+\s*(?:class|struct|interface|enum|record)\s+([A-Za-z_][A-Za-z0-9_]*)\b",
        )
        .expect("csharp type regex")
    });
    static NAMESPACE_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"(?m)^\s*namespace\s+([A-Za-z_][A-Za-z0-9_\\.]+)\s*[{;]")
            .expect("csharp namespace regex")
    });

    let mut out = Vec::new();
    for cap in TYPE_RE.captures_iter(text).take(256) {
        if let Some(name) = cap.get(1).map(|m| m.as_str()) {
            out.push(name.to_string());
        }
    }
    for cap in NAMESPACE_RE.captures_iter(text).take(64) {
        if let Some(ns) = cap.get(1).map(|m| m.as_str()) {
            out.push(ns.to_string());
        }
    }

    out
}

fn enrich_hits_with_context(
    index: &SearchIndex,
    project_root: &Path,
    spec: &unity_asset_search_core::QuerySpec,
    hits: &mut [SearchHit],
) {
    let query_terms = to_terms(&spec.free_text);
    let query_tokens: Vec<&str> = query_terms.split_whitespace().collect();
    if query_tokens.is_empty() {
        return;
    }

    let mut extracted: Vec<Option<(Vec<String>, Vec<String>)>> = Vec::with_capacity(hits.len());
    let mut needed_guids = std::collections::BTreeSet::<String>::new();

    for hit in hits.iter() {
        if !matches!(hit.kind.as_str(), "Prefab" | "Scene") {
            extracted.push(None);
            continue;
        }

        let Some((hierarchy_paths, script_guids)) =
            index.yaml_enrich_info_for_rel_path(project_root, &hit.path)
        else {
            extracted.push(None);
            continue;
        };

        for g in &script_guids {
            needed_guids.insert(g.clone());
        }
        extracted.push(Some((hierarchy_paths, script_guids)));
    }

    let script_symbols_by_guid: std::collections::HashMap<String, Vec<String>> = index
        .inner
        .read()
        .ok()
        .map(|inner| {
            needed_guids
                .into_iter()
                .filter_map(|guid| {
                    inner
                        .state
                        .scripts
                        .get(&guid)
                        .map(|e| (guid, e.symbols.clone()))
                })
                .collect()
        })
        .unwrap_or_default();

    for (hit, info) in hits.iter_mut().zip(extracted.into_iter()) {
        let Some((hierarchy_paths, script_guids)) = info else {
            continue;
        };

        hit.matched_hierarchy_paths = hierarchy_paths
            .into_iter()
            .filter(|p| matches_any_token(&to_terms(p), &query_tokens))
            .take(6)
            .collect();

        let mut matched_symbols = std::collections::BTreeSet::<String>::new();
        for guid in script_guids {
            let Some(symbols) = script_symbols_by_guid.get(&guid) else {
                continue;
            };
            for sym in symbols {
                if matched_symbols.len() >= 12 {
                    break;
                }
                if sym.trim().is_empty() {
                    continue;
                }
                if matches_any_token(&to_terms(sym), &query_tokens) {
                    matched_symbols.insert(sym.clone());
                }
            }
            if matched_symbols.len() >= 12 {
                break;
            }
        }
        hit.matched_script_symbols = matched_symbols.into_iter().collect();
    }
}

fn matches_any_token(haystack_terms: &str, tokens: &[&str]) -> bool {
    let haystack = haystack_terms.trim();
    if haystack.is_empty() {
        return false;
    }
    for t in tokens {
        if t.is_empty() {
            continue;
        }
        if haystack.contains(t) {
            return true;
        }
    }
    false
}

fn extract_unity_yaml_script_guids(text: &str) -> Vec<String> {
    static SCRIPT_GUID_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"m_Script:\s*\{[^}]*\bguid:\s*([0-9a-fA-F]{32})\b")
            .expect("m_Script guid regex")
    });

    let mut out = std::collections::BTreeSet::new();
    for cap in SCRIPT_GUID_RE.captures_iter(text).take(2048) {
        let Some(guid) = cap.get(1).map(|m| m.as_str()) else {
            continue;
        };
        out.insert(guid.to_string());
        if out.len() >= 256 {
            break;
        }
    }
    out.into_iter().collect()
}

fn is_probably_unity_yaml_text(text: &str) -> bool {
    text.contains("%YAML") || text.contains("!u!") || text.contains("\n---")
}

fn fingerprint_for_path(path: &Path) -> Result<Fingerprint> {
    let meta = fs::metadata(path)?;
    let size = meta.len();
    let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let mtime_ms = mtime
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);

    let (meta_size, meta_mtime_ms) = asset_meta_path(path)
        .and_then(|meta_path| fs::metadata(meta_path).ok())
        .map(|meta| {
            let size = meta.len();
            let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let mtime_ms = mtime
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            (size, mtime_ms)
        })
        .unwrap_or((0, 0));

    Ok(Fingerprint {
        size,
        mtime_ms,
        meta_size,
        meta_mtime_ms,
    })
}

fn asset_meta_path(asset_path: &Path) -> Option<PathBuf> {
    meta_path_for_asset(asset_path)
}

fn read_guid_from_meta(meta_path: Option<PathBuf>) -> Option<String> {
    static GUID_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"(?m)^guid:\s*([0-9a-fA-F]{32})\s*$").expect("guid regex")
    });

    let meta_path = meta_path?;
    let meta = fs::read_to_string(meta_path).ok()?;
    GUID_RE
        .captures(&meta)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str().to_string())
}

fn load_state(path: &Path) -> Result<IndexState> {
    let bytes = fs::read(path)?;
    let state = serde_json::from_slice(&bytes)?;
    Ok(state)
}

fn store_state(path: &Path, state: &IndexState) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state)?;
    atomic_write(path, &bytes)?;
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(io::Error::other("no parent dir"));
    };
    fs::create_dir_all(parent)?;

    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn is_excluded_dir(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        matches!(
            n,
            ".git"
                | "target"
                | "Library"
                | ".venv-unitypy"
                | ".unity-asset-search"
                | "unity-asset-search"
                | "Temp"
                | "Obj"
                | "Logs"
        )
    })
}

fn should_skip_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

fn classify_kind(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "prefab" => "Prefab",
        "unity" => "Scene",
        "mat" => "Material",
        "cs" => "Script",
        "anim" => "AnimationClip",
        "controller" => "AnimatorController",
        "asset" => "Asset",
        "shader" => "Shader",
        "png" | "jpg" | "jpeg" | "tga" | "psd" => "Texture",
        "wav" | "mp3" | "ogg" => "Audio",
        _ => "File",
    }
    .to_string()
}

fn meta_path_for_asset(asset_path: &Path) -> Option<PathBuf> {
    if !asset_path.is_file() {
        return None;
    }
    let Some(ext) = asset_path.extension().and_then(|e| e.to_str()) else {
        let meta = asset_path.with_extension("meta");
        return meta.exists().then_some(meta);
    };

    let meta = asset_path.with_extension(format!("{ext}.meta"));
    meta.exists().then_some(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_container_entries_are_indexed_when_enabled() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("Assets")).unwrap();

        let sample_bundle =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab");
        assert!(sample_bundle.is_file(), "missing test bundle sample");

        let dest_bundle = temp.path().join("Assets/sample.ab");
        fs::copy(&sample_bundle, &dest_bundle).unwrap();

        let paths = IndexPaths::for_project(temp.path().to_path_buf(), None, None).unwrap();
        let index = SearchIndex::open_or_create_with_options(
            &paths,
            SearchIndexOptions {
                index_bundle_container_entries: true,
                max_bundle_container_entries_per_bundle: 10_000,
                ..Default::default()
            },
        )
        .unwrap();
        index.reindex_full(&paths).unwrap();

        let extracted = extract_assetbundle_container_asset_paths(&dest_bundle, 10_000).unwrap();
        assert!(
            !extracted.is_empty(),
            "sample bundle had no container entries"
        );

        let needle = container_name_from_asset_path(&extracted[0]);
        let query = format!("type:bundlecontainer {needle}");
        let resp = index
            .search_enriched(paths.project_root.as_path(), &query, 50)
            .unwrap();

        assert!(
            resp.hits.iter().any(|h| h.kind == "BundleContainer"),
            "expected BundleContainer hits"
        );
        assert!(
            resp.hits
                .iter()
                .any(|h| h.kind == "BundleContainer" && h.location.path == "Assets/sample.ab"),
            "expected BundleContainer hit with location pointing at bundle"
        );
    }

    #[test]
    fn default_scan_roots_includes_unity_dirs_when_present() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("Assets")).unwrap();
        fs::create_dir_all(temp.path().join("Packages")).unwrap();
        fs::create_dir_all(temp.path().join("ProjectSettings")).unwrap();

        let roots = default_scan_roots(temp.path());
        assert_eq!(roots.len(), 3);
    }

    #[test]
    fn normalize_scan_roots_rejects_outside_project_root() {
        let temp = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("Assets")).unwrap();

        let err = normalize_scan_roots(temp.path(), vec![other.path().to_path_buf()]).unwrap_err();
        assert!(err.to_string().contains("inside project root"));
    }

    #[test]
    fn unity_yaml_extraction_picks_primary_name_and_guid() {
        let text = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: Player
--- !u!114 &2
MonoBehaviour:
  m_Script: {fileID: 11500000, guid: deadbeefdeadbeefdeadbeefdeadbeef, type: 3}
"#;

        let scripts = std::collections::BTreeMap::new();
        let extracted = extract_unity_yaml_content(text, &scripts);
        assert_eq!(extracted.primary_name.as_deref(), Some("Player"));
        let terms = extracted.content_terms.unwrap_or_default();
        assert!(terms.contains("player"));
        assert!(terms.contains("deadbeefdeadbeefdeadbeefdeadbeef"));
        assert!(terms.contains("11500000"));
    }

    #[test]
    fn unity_yaml_hierarchy_paths_build_root_child() {
        let text = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &10
GameObject:
  m_Name: Root
--- !u!4 &20
Transform:
  m_GameObject: {fileID: 10}
  m_Father: {fileID: 0}
--- !u!1 &11
GameObject:
  m_Name: Child
--- !u!4 &21
Transform:
  m_GameObject: {fileID: 11}
  m_Father: {fileID: 20}
"#;

        let paths = extract_unity_yaml_hierarchy_paths(text);
        assert!(paths.iter().any(|p| p == "Root/Child"));
    }

    #[test]
    fn group_reference_contexts_merges_hints_for_same_object() {
        let a = ReferenceContext {
            doc_file_id: Some(10),
            doc_class_id: Some(1),
            object_name: Some("Player".to_string()),
            hierarchy_path: Some("Root/Player".to_string()),
            field_hint: Some("m_Material".to_string()),
            source_line: None,
            source_column: None,
        };
        let b = ReferenceContext {
            doc_file_id: Some(10),
            doc_class_id: Some(1),
            object_name: Some("Player".to_string()),
            hierarchy_path: Some("Root/Player".to_string()),
            field_hint: Some("m_Materials[0]".to_string()),
            source_line: None,
            source_column: None,
        };

        let (contexts, objects) =
            group_reference_contexts_and_objects(vec![b, a], "Assets/a.prefab", None);
        assert_eq!(contexts.len(), 1);
        assert_eq!(
            contexts[0].field_hint.as_deref(),
            Some("m_Material, m_Materials[0]")
        );
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].location.path, "Assets/a.prefab");
        assert_eq!(objects[0].location.file_id, Some(10));
        assert_eq!(objects[0].location.class_id, Some(1));
        assert_eq!(objects[0].field_hints, vec!["m_Material", "m_Materials[0]"]);
    }

    #[test]
    fn group_reference_contexts_keeps_objects_separate() {
        let a = ReferenceContext {
            doc_file_id: Some(10),
            doc_class_id: Some(1),
            object_name: Some("A".to_string()),
            hierarchy_path: Some("Root/A".to_string()),
            field_hint: None,
            source_line: None,
            source_column: None,
        };
        let b = ReferenceContext {
            doc_file_id: Some(11),
            doc_class_id: Some(1),
            object_name: Some("B".to_string()),
            hierarchy_path: Some("Root/B".to_string()),
            field_hint: Some("m_Script".to_string()),
            source_line: None,
            source_column: None,
        };

        let (contexts, objects) =
            group_reference_contexts_and_objects(vec![a, b], "Assets/a.prefab", None);
        assert_eq!(contexts.len(), 2);
        assert!(
            contexts
                .iter()
                .any(|c| c.object_name.as_deref() == Some("A"))
        );
        assert!(
            contexts
                .iter()
                .any(|c| c.object_name.as_deref() == Some("B"))
        );
        assert_eq!(objects.len(), 2);
        assert!(
            objects
                .iter()
                .any(|c| c.object_name.as_deref() == Some("A"))
        );
        assert!(
            objects
                .iter()
                .any(|c| c.object_name.as_deref() == Some("B"))
        );
    }

    #[test]
    fn stable_id_prefers_guid_and_appends_file_id() {
        assert_eq!(
            stable_id_for(
                Some("DEADBEEFDEADBEEFDEADBEEFDEADBEEF"),
                "Assets/a.prefab",
                Some(10)
            ),
            "guid:deadbeefdeadbeefdeadbeefdeadbeef#10"
        );
        assert_eq!(
            stable_id_for(None, "Assets/a.prefab", Some(10)),
            "path:Assets/a.prefab#10"
        );
    }

    #[test]
    fn status_reports_project_ignore_files_present() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("Assets")).unwrap();
        fs::write(temp.path().join(".unity-asset-search-ignore"), "Library/\n").unwrap();

        let paths = IndexPaths::for_project(temp.path().to_path_buf(), None, None).unwrap();
        let index = SearchIndex::open_or_create(&paths).unwrap();
        let status = index.status().unwrap();

        assert!(
            status
                .ignore_files_supported
                .iter()
                .any(|n| n == ".unity-asset-search-ignore")
        );
        assert!(
            status
                .project_ignore_files_present
                .iter()
                .any(|n| n == ".unity-asset-search-ignore")
        );
    }

    #[test]
    fn reindex_changed_paths_removes_directory_prefix() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("Assets/Dir")).unwrap();
        fs::create_dir_all(temp.path().join("Packages")).unwrap();
        fs::create_dir_all(temp.path().join("ProjectSettings")).unwrap();

        let prefab = temp.path().join("Assets/Dir/foo.prefab");
        fs::write(
            &prefab,
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Foo\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("Assets/Dir/foo.prefab.meta"),
            "fileFormatVersion: 2\nguid: deadbeefdeadbeefdeadbeefdeadbeef\n",
        )
        .unwrap();

        let paths = IndexPaths::for_project(temp.path().to_path_buf(), None, None).unwrap();
        let index = SearchIndex::open_or_create(&paths).unwrap();
        index.reindex_full(&paths).unwrap();
        let status = index.status().unwrap();
        assert_eq!(status.indexed_docs, 1);

        fs::remove_dir_all(temp.path().join("Assets/Dir")).unwrap();
        index
            .reindex_changed_paths(&paths, &[temp.path().join("Assets/Dir")])
            .unwrap();
        let status = index.status().unwrap();
        assert_eq!(status.indexed_docs, 0);
        assert_eq!(status.removed_docs, Some(1));
    }

    #[test]
    fn normalize_watch_paths_dedupes_meta_and_asset() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("Assets")).unwrap();

        let prefab = temp.path().join("Assets/foo.prefab");
        fs::write(
            &prefab,
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Foo\n",
        )
        .unwrap();
        let meta = temp.path().join("Assets/foo.prefab.meta");
        fs::write(
            &meta,
            "fileFormatVersion: 2\nguid: deadbeefdeadbeefdeadbeefdeadbeef\n",
        )
        .unwrap();

        let paths = IndexPaths::for_project(temp.path().to_path_buf(), None, None).unwrap();
        let index = SearchIndex::open_or_create(&paths).unwrap();
        index.reindex_full(&paths).unwrap();

        let inner = index.inner.read().unwrap();
        let normalized =
            normalize_watch_paths_for_incremental(&paths, &inner.state, &[meta, prefab.clone()]);
        assert_eq!(normalized, vec![prefab]);
    }

    #[test]
    fn reindex_changed_paths_handles_directory_rename() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("Assets/A")).unwrap();

        let prefab = temp.path().join("Assets/A/foo.prefab");
        fs::write(
            &prefab,
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Foo\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("Assets/A/foo.prefab.meta"),
            "fileFormatVersion: 2\nguid: deadbeefdeadbeefdeadbeefdeadbeef\n",
        )
        .unwrap();

        let paths = IndexPaths::for_project(temp.path().to_path_buf(), None, None).unwrap();
        let index = SearchIndex::open_or_create(&paths).unwrap();
        index.reindex_full(&paths).unwrap();

        fs::create_dir_all(temp.path().join("Assets")).unwrap();
        fs::rename(temp.path().join("Assets/A"), temp.path().join("Assets/B")).unwrap();

        index
            .reindex_changed_paths(
                &paths,
                &[temp.path().join("Assets/A"), temp.path().join("Assets/B")],
            )
            .unwrap();

        let status = index.status().unwrap();
        assert_eq!(status.indexed_docs, 1);

        let res_old = index.search("in:Assets/A", 20).unwrap();
        assert_eq!(res_old.hits.len(), 0);

        let res_new = index.search("in:Assets/B", 20).unwrap();
        assert_eq!(res_new.hits.len(), 1);
        assert!(res_new.hits[0].path.starts_with("Assets/B/"));
    }

    #[test]
    fn meta_guid_changes_trigger_doc_guid_update() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("Assets")).unwrap();

        let prefab = temp.path().join("Assets/foo.prefab");
        fs::write(
            &prefab,
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Foo\n",
        )
        .unwrap();
        let meta = temp.path().join("Assets/foo.prefab.meta");
        fs::write(
            &meta,
            "fileFormatVersion: 2\nguid: deadbeefdeadbeefdeadbeefdeadbeef\n",
        )
        .unwrap();

        let paths = IndexPaths::for_project(temp.path().to_path_buf(), None, None).unwrap();
        let index = SearchIndex::open_or_create(&paths).unwrap();
        index.reindex_full(&paths).unwrap();

        fs::write(
            &meta,
            "fileFormatVersion: 2\nguid: cafe0000cafe0000cafe0000cafe0000\n",
        )
        .unwrap();

        index
            .reindex_changed_paths(&paths, std::slice::from_ref(&meta))
            .unwrap();

        let inner = index.inner.read().unwrap();
        let searcher = inner.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(inner.fields.id, "Assets/foo.prefab"),
            tantivy::schema::IndexRecordOption::Basic,
        );
        let hits = searcher.search(&query, &TopDocs::with_limit(5)).unwrap();
        assert_eq!(hits.len(), 1);
        let doc: TantivyDocument = searcher.doc(hits[0].1).unwrap();
        let stored = doc
            .get_first(inner.fields.guid)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        assert_eq!(stored, "cafe0000cafe0000cafe0000cafe0000");
    }
}
