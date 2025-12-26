use std::fs;
use std::io;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use ignore::{DirEntry, WalkBuilder};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, Occur, PhrasePrefixQuery, Query, TermQuery,
};
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, Value as _};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};

use unity_asset_search_core::{
    MatchKind, highlight_html, normalize_for_match, parse_query, rank_match, to_terms,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub guid: Option<String>,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub score: f32,
    pub match_kind: MatchKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_hierarchy_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_script_symbols: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight_name: Option<String>,
    #[serde(skip_serializing)]
    rank_fuzzy_score: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub took_ms: u128,
    pub total_hits: usize,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestResponse {
    pub prefix: String,
    pub took_ms: u128,
    pub suggestions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub index_root_dir: PathBuf,
    pub index_data_dir: PathBuf,
    pub scan_roots: Vec<PathBuf>,
    pub indexed_docs: u64,
    pub indexed_scripts: u64,
    pub last_index_duration_ms: Option<u128>,
    pub indexing: bool,
    pub last_scan_ms: Option<u128>,
    pub updated_docs: Option<u64>,
    pub removed_docs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct IndexPaths {
    pub project_root: PathBuf,
    pub index_root_dir: PathBuf,
    pub index_data_dir: PathBuf,
    pub scan_roots: Vec<PathBuf>,
    pub state_path: PathBuf,
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
        let state_path = index_root_dir.join("state-v2.json");

        Ok(Self {
            project_root,
            index_root_dir,
            index_data_dir,
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
}

struct SearchIndexInner {
    reader: IndexReader,
    writer: IndexWriter,
    fields: SearchFields,
    status: StatusResponse,
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
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct Fingerprint {
    size: u64,
    mtime_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct IndexState {
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

impl SearchIndex {
    pub fn open_or_create(paths: &IndexPaths) -> Result<Self> {
        fs::create_dir_all(&paths.index_root_dir).with_context(|| {
            format!("create index root dir: {}", paths.index_root_dir.display())
        })?;
        fs::create_dir_all(&paths.index_data_dir).with_context(|| {
            format!("create index data dir: {}", paths.index_data_dir.display())
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

        let state = load_state(&paths.state_path).unwrap_or_default();

        let status = StatusResponse {
            index_root_dir: paths.index_root_dir.clone(),
            index_data_dir: paths.index_data_dir.clone(),
            scan_roots: paths.scan_roots.clone(),
            indexed_docs: 0,
            indexed_scripts: 0,
            last_index_duration_ms: None,
            indexing: false,
            last_scan_ms: None,
            updated_docs: None,
            removed_docs: None,
        };

        let this = Self {
            inner: Arc::new(RwLock::new(SearchIndexInner {
                reader,
                writer,
                fields,
                status,
                state,
            })),
        };

        this.refresh_status()?;
        Ok(this)
    }

    pub fn status(&self) -> Result<StatusResponse> {
        self.refresh_status()?;
        Ok(self
            .inner
            .read()
            .map_err(|_| anyhow!("poisoned lock"))?
            .status
            .clone())
    }

    pub fn reindex(&self, paths: &IndexPaths) -> Result<()> {
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
        let start = Instant::now();
        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
            inner.status.indexing = true;
            inner.status.updated_docs = None;
            inner.status.removed_docs = None;
            inner.status.last_scan_ms = None;
        }

        let scan_start = Instant::now();
        let delta = scan_changed_paths(paths, changed_paths)?;
        let scan_ms = scan_start.elapsed().as_millis();

        let fields = {
            let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
            inner.fields.clone()
        };

        let mut updated_docs = 0u64;
        let mut removed_docs = 0u64;

        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
            let mut state = inner.state.clone();
            let mut scripts = state.scripts.clone();

            for removed in &delta.removed_rel_paths {
                inner
                    .writer
                    .delete_term(Term::from_field_text(fields.id, removed));
                if state.files.remove(removed).is_some() {
                    removed_docs += 1;
                }
                remove_script_entries_for_rel_path(&mut scripts, removed);
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
                inner
                    .writer
                    .add_document(build_doc(&fields, file, &scripts)?)?;
                state.files.insert(file.rel_path.clone(), file.fingerprint);
                updated_docs += 1;
            }

            inner.writer.commit()?;
            state.scripts = scripts;
            inner.state = state;
            store_state(&paths.state_path, &inner.state)?;
        }

        self.inner
            .read()
            .map_err(|_| anyhow!("poisoned lock"))?
            .reader
            .reload()?;

        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
            inner.status.last_index_duration_ms = Some(start.elapsed().as_millis());
            inner.status.last_scan_ms = Some(scan_ms);
            inner.status.updated_docs = Some(updated_docs);
            inner.status.removed_docs = Some(removed_docs);
            inner.status.indexing = false;
        }

        self.refresh_status()?;
        Ok(())
    }

    fn reindex_impl(&self, paths: &IndexPaths, mode: ReindexMode) -> Result<()> {
        let start = Instant::now();
        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
            inner.status.indexing = true;
            inner.status.updated_docs = None;
            inner.status.removed_docs = None;
            inner.status.last_scan_ms = None;
        }

        let scan_start = Instant::now();
        let scan = scan_project_files(paths)?;
        let scan_ms = scan_start.elapsed().as_millis();

        let (fields, mut state) = {
            let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
            (inner.fields.clone(), inner.state.clone())
        };

        let scripts = build_script_guid_map(&scan, &state.scripts)?;

        let mut updated_docs = 0u64;
        let mut removed_docs = 0u64;

        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;

            if mode == ReindexMode::Full {
                inner.writer.delete_all_documents()?;
                state.files.clear();
                state.scripts.clear();
            }

            for removed in state
                .files
                .keys()
                .filter(|path| !scan.files.contains_key(*path))
                .cloned()
                .collect::<Vec<_>>()
            {
                inner
                    .writer
                    .delete_term(Term::from_field_text(fields.id, &removed));
                state.files.remove(&removed);
                removed_docs += 1;
            }

            for (rel_path, file) in &scan.files {
                let old = state.files.get(rel_path).copied();
                if old == Some(file.fingerprint) && mode == ReindexMode::Incremental {
                    continue;
                }

                inner
                    .writer
                    .delete_term(Term::from_field_text(fields.id, rel_path));
                inner
                    .writer
                    .add_document(build_doc(&fields, file, &scripts)?)?;
                state.files.insert(rel_path.clone(), file.fingerprint);
                updated_docs += 1;
            }

            inner.writer.commit()?;
            state.scripts = scripts;
            inner.state = state;
            store_state(&paths.state_path, &inner.state)?;
        }

        self.inner
            .read()
            .map_err(|_| anyhow!("poisoned lock"))?
            .reader
            .reload()?;

        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
            inner.status.last_index_duration_ms = Some(start.elapsed().as_millis());
            inner.status.last_scan_ms = Some(scan_ms);
            inner.status.updated_docs = Some(updated_docs);
            inner.status.removed_docs = Some(removed_docs);
            inner.status.indexing = false;
        }

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

            let rank_query = if spec.free_text.is_empty() {
                spec.raw.as_str()
            } else {
                spec.free_text.as_str()
            };
            let rank = rank_match(rank_query, &name, &path);

            hits.push(SearchHit {
                guid,
                path,
                name,
                kind,
                score: bm25,
                match_kind: rank.kind,
                matched_hierarchy_paths: Vec::new(),
                matched_script_symbols: Vec::new(),
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

            let rank_query = if spec.free_text.is_empty() {
                spec.raw.as_str()
            } else {
                spec.free_text.as_str()
            };
            let rank = rank_match(rank_query, &name, &path);

            hits.push(SearchHit {
                guid,
                path,
                name,
                kind,
                score: bm25,
                match_kind: rank.kind,
                matched_hierarchy_paths: Vec::new(),
                matched_script_symbols: Vec::new(),
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

    fn refresh_status(&self) -> Result<()> {
        let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
        let searcher = inner.reader.searcher();

        let mut status = inner.status.clone();
        status.indexed_docs = searcher.num_docs();
        status.indexed_scripts = inner.state.scripts.len() as u64;

        drop(inner);
        self.inner
            .write()
            .map_err(|_| anyhow!("poisoned lock"))?
            .status = status;

        Ok(())
    }
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
        content_terms: schema.get_field("content_terms").expect("content_terms field"),
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

fn scan_project_files(paths: &IndexPaths) -> Result<ScanResult> {
    let mut out = ScanResult::default();

    let walker = build_project_walker(paths)?;
    for entry in walker {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }

        let path = entry.path();
        if path.extension().is_some_and(|e| e == "meta") {
            continue;
        }

        if should_skip_file(path) || is_excluded_dir(path) || !is_in_scan_roots(paths, path) {
            continue;
        }

        let rel_path = path
            .strip_prefix(&paths.project_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();

        let kind = classify_kind(path);
        let fingerprint = fingerprint_for_path(path)?;

        let file = ScannedFile {
            rel_path: rel_path.clone(),
            abs_path: path.to_path_buf(),
            fingerprint,
            name,
            kind,
        };

        out.files.insert(rel_path, file);
    }

    Ok(out)
}

#[derive(Debug, Clone, Default)]
struct ChangeScanResult {
    files: Vec<ScannedFile>,
    removed_rel_paths: Vec<String>,
}

fn scan_changed_paths(paths: &IndexPaths, changed_paths: &[PathBuf]) -> Result<ChangeScanResult> {
    let mut out = ChangeScanResult::default();
    if changed_paths.is_empty() {
        return Ok(out);
    }

    let mut candidates = Vec::new();
    for p in changed_paths {
        if p.starts_with(&paths.index_root_dir) {
            continue;
        }
        if p.extension().is_some_and(|e| e == "meta") {
            if let Some(asset) = asset_path_from_meta(p) {
                candidates.push(asset);
            }
        } else {
            candidates.push(p.clone());
        }
    }

    candidates.sort();
    candidates.dedup();

    let mut existing = Vec::new();
    for candidate in candidates {
        if !candidate.starts_with(&paths.project_root) {
            continue;
        }
        if should_skip_file(&candidate) || is_excluded_dir(&candidate) {
            continue;
        }
        if candidate.is_file() {
            existing.push(candidate);
        } else if let Ok(rel) = candidate.strip_prefix(&paths.project_root) {
            out.removed_rel_paths
                .push(rel.to_string_lossy().to_string());
        }
    }

    if existing.is_empty() {
        return Ok(out);
    }

    let existing_set: std::collections::BTreeSet<PathBuf> = existing.into_iter().collect();
    let allowed_dirs = build_allowed_dirs(paths, &existing_set);
    let existing_set_for_filter = existing_set.clone();

    let scan_roots = paths.scan_roots.clone();
    let project_root = paths.project_root.clone();

    let mut builder = WalkBuilder::new(&project_root);
    builder
        .follow_links(false)
        .parents(false)
        .ignore(true)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .add_custom_ignore_filename(".gitignore")
        .filter_entry(move |e: &DirEntry| {
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

    for entry in builder.build() {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }

        let path = entry.path();
        if should_skip_file(path) || is_excluded_dir(path) || !existing_set.contains(path) {
            continue;
        }

        let rel_path = path
            .strip_prefix(&paths.project_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let kind = classify_kind(path);
        let fingerprint = fingerprint_for_path(path)?;

        out.files.push(ScannedFile {
            rel_path,
            abs_path: path.to_path_buf(),
            fingerprint,
            name,
            kind,
        });
    }

    Ok(out)
}

fn build_project_walker(paths: &IndexPaths) -> Result<ignore::Walk> {
    let scan_roots = paths.scan_roots.clone();
    let project_root = paths.project_root.clone();

    let mut builder = WalkBuilder::new(&project_root);
    builder
        .follow_links(false)
        .parents(false)
        .ignore(true)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .add_custom_ignore_filename(".gitignore")
        .filter_entry(move |e: &DirEntry| {
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

    Ok(builder.build())
}

fn is_in_scan_roots(paths: &IndexPaths, path: &Path) -> bool {
    is_in_scan_roots_raw(&paths.scan_roots, path)
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
            (script_terms_for_source(file, text), extract_csharp_symbols(text))
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
        (script_terms_for_source(file, text), extract_csharp_symbols(text))
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
    to_terms(&format!("{} {} {}", file.name, symbols.join(" "), file.rel_path))
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

    if matches!(file.kind.as_str(), "Prefab" | "Scene" | "Material" | "Asset")
        && is_probably_unity_yaml(&file.abs_path)?
    {
        let text = read_text_limited(&file.abs_path, 2 * 1024 * 1024)?;
        let Some(text) = text else {
            return Ok(ExtractedContent::default());
        };
        return Ok(extract_unity_yaml_content(&text, scripts));
    }

    if matches!(
        ext.as_str(),
        "cs"
            | "shader"
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
    static FILEID_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"\bfileID:\s*([0-9]+)\b").expect("fileID regex")
    });

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

    for path in extract_unity_yaml_hierarchy_paths(text).into_iter().take(512) {
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

    let mut current = transforms.get(&transform_id).and_then(|t| t.father_transform_id);
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

    for hit in hits {
        if hit.path.trim().is_empty() {
            continue;
        }
        let abs = project_root.join(&hit.path);
        let Ok(Some(text)) = read_text_limited(&abs, 2 * 1024 * 1024) else {
            continue;
        };
        if !is_probably_unity_yaml_text(&text) {
            continue;
        }

        let mut matched_paths = Vec::new();
        for path in extract_unity_yaml_hierarchy_paths(&text) {
            if matched_paths.len() >= 6 {
                break;
            }
            if matches_any_token(&to_terms(&path), &query_tokens) {
                matched_paths.push(path);
            }
        }
        hit.matched_hierarchy_paths = matched_paths;

        let mut matched_symbols = std::collections::BTreeSet::<String>::new();
        for guid in extract_unity_yaml_script_guids(&text) {
            if matched_symbols.len() >= 12 {
                break;
            }
            let Some(symbols) = index
                .inner
                .read()
                .ok()
                .and_then(|inner| inner.state.scripts.get(&guid).map(|e| e.symbols.clone()))
            else {
                continue;
            };

            for sym in symbols {
                if sym.trim().is_empty() {
                    continue;
                }
                if matches_any_token(&to_terms(&sym), &query_tokens) {
                    matched_symbols.insert(sym);
                }
                if matched_symbols.len() >= 12 {
                    break;
                }
            }
        }
        hit.matched_script_symbols = matched_symbols.into_iter().take(12).collect();
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

    Ok(Fingerprint { size, mtime_ms })
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
}
