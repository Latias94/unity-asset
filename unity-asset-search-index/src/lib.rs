use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, Value as _};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, doc};
use walkdir::WalkDir;

use unity_asset_search_core::{MatchKind, rank_match, to_terms};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub guid: Option<String>,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub score: f32,
    pub match_kind: MatchKind,
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
pub struct StatusResponse {
    pub index_dir: PathBuf,
    pub indexed_docs: u64,
    pub last_index_duration_ms: Option<u128>,
    pub indexing: bool,
}

#[derive(Debug, Clone)]
pub struct IndexPaths {
    pub project_root: PathBuf,
    pub index_dir: PathBuf,
    pub scan_roots: Vec<PathBuf>,
}

impl IndexPaths {
    pub fn for_project(project_root: PathBuf, index_dir: Option<PathBuf>) -> Result<Self> {
        let index_dir = match index_dir {
            Some(p) => p,
            None => default_index_dir(&project_root),
        };

        let assets_dir = project_root.join("Assets");
        let scan_roots = if assets_dir.is_dir() {
            vec![assets_dir]
        } else {
            vec![project_root.clone()]
        };

        Ok(Self {
            project_root,
            index_dir,
            scan_roots,
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

#[derive(Clone)]
pub struct SearchIndex {
    inner: Arc<RwLock<SearchIndexInner>>,
}

struct SearchIndexInner {
    index: Index,
    reader: IndexReader,
    writer: IndexWriter,
    fields: SearchFields,
    status: StatusResponse,
}

#[derive(Clone)]
struct SearchFields {
    guid: Field,
    path: Field,
    path_terms: Field,
    name: Field,
    name_terms: Field,
    kind: Field,
    kind_terms: Field,
}

impl SearchIndex {
    pub fn open_or_create(paths: &IndexPaths) -> Result<Self> {
        fs::create_dir_all(&paths.index_dir)
            .with_context(|| format!("create index dir: {}", paths.index_dir.display()))?;

        let schema = build_schema();
        let index = Index::open_in_dir(&paths.index_dir)
            .or_else(|_| Index::create_in_dir(&paths.index_dir, schema.clone()))?;

        let schema = index.schema();
        let fields = build_fields(&schema);
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        let writer = index
            .writer_with_num_threads(4, 128 * 1024 * 1024)
            .context("create index writer")?;

        let status = StatusResponse {
            index_dir: paths.index_dir.clone(),
            indexed_docs: 0,
            last_index_duration_ms: None,
            indexing: false,
        };

        let this = Self {
            inner: Arc::new(RwLock::new(SearchIndexInner {
                index,
                reader,
                writer,
                fields,
                status,
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
        let start = Instant::now();
        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
            inner.status.indexing = true;
        }

        let fields = {
            self.inner
                .read()
                .map_err(|_| anyhow!("poisoned lock"))?
                .fields
                .clone()
        };
        let docs = scan_project_docs(paths, &fields)?;
        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
            inner.writer.delete_all_documents()?;
            for doc in docs {
                inner.writer.add_document(doc)?;
            }
            inner.writer.commit()?;
        }

        self.inner
            .read()
            .map_err(|_| anyhow!("poisoned lock"))?
            .reader
            .reload()?;

        {
            let mut inner = self.inner.write().map_err(|_| anyhow!("poisoned lock"))?;
            inner.status.last_index_duration_ms = Some(start.elapsed().as_millis());
            inner.status.indexing = false;
        }

        self.refresh_status()?;
        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<SearchResponse> {
        let start = Instant::now();
        let query = query.trim();
        if query.is_empty() {
            return Ok(SearchResponse {
                query: String::new(),
                took_ms: 0,
                total_hits: 0,
                hits: Vec::new(),
            });
        }

        let inner = self.inner.read().map_err(|_| anyhow!("poisoned lock"))?;
        let searcher = inner.reader.searcher();
        let query_parser = QueryParser::for_index(
            &inner.index,
            vec![
                inner.fields.name_terms,
                inner.fields.path_terms,
                inner.fields.kind_terms,
            ],
        );

        let parsed = query_parser.parse_query(query)?;
        let top_docs = searcher.search(&parsed, &TopDocs::with_limit(limit * 5))?;

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

            let rank = rank_match(query, &name, &path);

            hits.push(SearchHit {
                guid,
                path,
                name,
                kind,
                score: bm25,
                match_kind: rank.kind,
                rank_fuzzy_score: rank.fuzzy_score,
            });
        }

        hits.sort_by(|a, b| {
            (a.match_kind as u8, -a.rank_fuzzy_score, -a.score)
                .partial_cmp(&(b.match_kind as u8, -b.rank_fuzzy_score, -b.score))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        hits.truncate(limit);

        Ok(SearchResponse {
            query: query.to_string(),
            took_ms: start.elapsed().as_millis(),
            total_hits: hits.len(),
            hits,
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

fn build_schema() -> Schema {
    let mut builder = Schema::builder();
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
        guid: schema.get_field("guid").expect("guid field"),
        path: schema.get_field("path").expect("path field"),
        path_terms: schema.get_field("path_terms").expect("path_terms field"),
        name: schema.get_field("name").expect("name field"),
        name_terms: schema.get_field("name_terms").expect("name_terms field"),
        kind: schema.get_field("kind").expect("kind field"),
        kind_terms: schema.get_field("kind_terms").expect("kind_terms field"),
    }
}

fn scan_project_docs(paths: &IndexPaths, fields: &SearchFields) -> Result<Vec<TantivyDocument>> {
    let guid_re = Regex::new(r"(?m)^guid:\s*([0-9a-fA-F]{32})\s*$")?;
    let mut docs = Vec::new();

    for root in &paths.scan_roots {
        let walker = WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !is_excluded_dir(e.path()));

        for entry in walker.filter_map(Result::ok) {
            if entry.file_type().is_dir() {
                continue;
            }

            let path = entry.path();
            if path.extension().is_some_and(|e| e == "meta") {
                continue;
            }

            if should_skip_file(path) {
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

            let guid = meta_path_for_asset(path)
                .and_then(|meta| fs::read_to_string(meta).ok())
                .and_then(|meta| {
                    guid_re
                        .captures(&meta)
                        .and_then(|cap| cap.get(1))
                        .map(|m| m.as_str().to_string())
                });

            docs.push(doc!(
                fields.guid => guid.clone().unwrap_or_default(),
                fields.path => rel_path.clone(),
                fields.path_terms => to_terms(&rel_path),
                fields.name => name.clone(),
                fields.name_terms => to_terms(&name),
                fields.kind => kind.clone(),
                fields.kind_terms => to_terms(&kind),
            ));
        }
    }

    Ok(docs)
}

fn is_excluded_dir(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        matches!(
            n,
            ".git"
                | "target"
                | "Library"
                | "repo-ref"
                | ".venv-unitypy"
                | ".unity-asset-search"
                | "unity-asset-search"
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
