use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, Occur, PhrasePrefixQuery, Query, TermQuery,
};
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, Value as _};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term, doc};

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

        let index_data_dir = index_root_dir.join("tantivy-v1");
        let state_path = index_root_dir.join("state-v1.json");

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
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct Fingerprint {
    size: u64,
    mtime_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct IndexState {
    files: std::collections::BTreeMap<String, Fingerprint>,
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

        let mut updated_docs = 0u64;
        let mut removed_docs = 0u64;

        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;

            if mode == ReindexMode::Full {
                inner.writer.delete_all_documents()?;
                state.files.clear();
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
                inner.writer.add_document(build_doc(&fields, file)?)?;
                state.files.insert(rel_path.clone(), file.fingerprint);
                updated_docs += 1;
            }

            inner.writer.commit()?;
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

    for root in &paths.scan_roots {
        let walker = WalkBuilder::new(root)
            .follow_links(false)
            .standard_filters(true)
            .filter_entry(|e| !is_excluded_dir(e.path()))
            .build();

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

            if should_skip_file(path) || is_excluded_dir(path) {
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
    }

    Ok(out)
}

fn build_doc(fields: &SearchFields, file: &ScannedFile) -> Result<TantivyDocument> {
    let guid = read_guid_from_meta(asset_meta_path(&file.abs_path)).unwrap_or_default();

    Ok(doc!(
        fields.id => file.rel_path.clone(),
        fields.guid => guid,
        fields.path => file.rel_path.clone(),
        fields.path_terms => to_terms(&file.rel_path),
        fields.name => file.name.clone(),
        fields.name_terms => to_terms(&file.name),
        fields.kind => file.kind.clone(),
        fields.kind_terms => to_terms(&file.kind),
    ))
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
}
