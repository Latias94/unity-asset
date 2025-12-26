use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use clap::Parser;
use notify::Watcher as _;
use rand::TryRngCore;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use unity_asset_search_index::{IndexPaths, SearchIndex};

#[derive(Debug, Parser)]
#[command(name = "unity-asset-search-daemon")]
struct Args {
    #[arg(long)]
    project_root: PathBuf,

    #[arg(long)]
    index_dir: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    scan_root: Vec<PathBuf>,

    #[arg(long)]
    scan_all: bool,

    #[arg(long, default_value = "127.0.0.1:9781")]
    listen: SocketAddr,

    #[arg(long)]
    token: Option<String>,

    #[arg(long)]
    no_auto_reindex: bool,

    #[arg(long)]
    watch: bool,

    #[arg(long, default_value_t = 1500)]
    watch_debounce_ms: u64,
}

#[derive(Clone)]
struct AppState {
    index: SearchIndex,
    token: String,
    paths: IndexPaths,
    reindex_lock: Arc<tokio::sync::Mutex<()>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let scan_roots = if args.scan_all {
        Some(vec![PathBuf::from(".")])
    } else if args.scan_root.is_empty() {
        None
    } else {
        Some(args.scan_root.clone())
    };
    let paths = IndexPaths::for_project(args.project_root, args.index_dir, scan_roots)?;
    let index = SearchIndex::open_or_create(&paths)?;

    let token = args.token.unwrap_or_else(generate_token);
    persist_token(&paths.index_root_dir, &token)?;

    eprintln!(
        "unity-asset-search-daemon listening on {} (index: {}, token: {})",
        args.listen,
        paths.index_root_dir.display(),
        token
    );

    let state = AppState {
        index: index.clone(),
        token,
        paths: paths.clone(),
        reindex_lock: Arc::new(tokio::sync::Mutex::new(())),
    };

    if !args.no_auto_reindex {
        let status = index.status()?;
        if status.indexed_docs == 0 && !status.indexing {
            let state = Arc::new(state.clone());
            tokio::spawn(async move {
                let _ = run_reindex(state, false).await;
            });
        }
    }

    if args.watch {
        let state = Arc::new(state.clone());
        let debounce = Duration::from_millis(args.watch_debounce_ms.max(100));
        tokio::spawn(async move {
            if let Err(err) = watch_and_reindex(state, debounce).await {
                eprintln!("watch error: {err}");
            }
        });
    }

    let app = axum::Router::new()
        .route("/v1/search", get(search))
        .route("/v1/status", get(status))
        .route("/v1/suggest", get(suggest))
        .route("/v1/references", get(references))
        .route("/v1/reindex", post(reindex))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(Arc::new(state));

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

async fn search(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    let index = state.index.clone();
    let project_root = state.paths.project_root.clone();
    let query = q.q.clone();
    match tokio::task::spawn_blocking(move || index.search_enriched(&project_root, &query, limit))
        .await
    {
        Ok(Ok(resp)) => (StatusCode::OK, Json(resp)).into_response(),
        Ok(Err(err)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let index = state.index.clone();
    match tokio::task::spawn_blocking(move || index.status()).await {
        Ok(Ok(resp)) => (StatusCode::OK, Json(resp)).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, serde::Deserialize)]
struct SuggestQuery {
    prefix: Option<String>,
    limit: Option<usize>,
}

async fn suggest(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SuggestQuery>,
) -> impl IntoResponse {
    let prefix = q.prefix.unwrap_or_default();
    let limit = q.limit.unwrap_or(10).clamp(1, 50);
    let index = state.index.clone();
    match tokio::task::spawn_blocking(move || index.suggest(&prefix, limit)).await {
        Ok(Ok(resp)) => (StatusCode::OK, Json(resp)).into_response(),
        Ok(Err(err)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, serde::Deserialize)]
struct ReferencesQuery {
    guid: String,
    file_id: Option<u64>,
    limit: Option<usize>,
}

async fn references(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ReferencesQuery>,
) -> impl IntoResponse {
    let guid = q.guid.clone();
    let file_id = q.file_id;
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let index = state.index.clone();
    let project_root = state.paths.project_root.clone();

    match tokio::task::spawn_blocking(move || {
        index.references_enriched(&project_root, &guid, file_id, limit)
    })
    .await
    {
        Ok(Ok(resp)) => (StatusCode::OK, Json(resp)).into_response(),
        Ok(Err(err)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, serde::Deserialize)]
struct ReindexParams {
    full: Option<bool>,
    #[serde(default)]
    path: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
}

async fn reindex(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ReindexParams>,
) -> impl IntoResponse {
    if !is_authorized(&headers, &state.token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "unauthorized" })),
        )
            .into_response();
    }

    let full = q.full.unwrap_or(false);
    let paths: Vec<String> = q.path.into_iter().chain(q.paths.into_iter()).collect();

    match if full {
        run_reindex(state, true).await
    } else if paths.is_empty() {
        run_reindex(state, false).await
    } else {
        run_reindex_paths(state, &paths).await
    } {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

fn is_authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    value == format!("Bearer {token}")
}

fn generate_token() -> String {
    let mut bytes = [0u8; 16];
    let mut rng = rand::rngs::OsRng;
    rng.try_fill_bytes(&mut bytes)
        .expect("OsRng should be available");
    hex::encode(bytes)
}

fn persist_token(index_dir: &std::path::Path, token: &str) -> anyhow::Result<()> {
    let path = index_dir.join("token");
    std::fs::write(path, token)?;
    Ok(())
}

async fn watch_and_reindex(state: Arc<AppState>, debounce: Duration) -> anyhow::Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WatchMsg>();

    let scan_roots = state.paths.scan_roots.clone();
    let index_root = state.paths.index_root_dir.clone();

    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            let Ok(event) = res else {
                return;
            };

            let mut paths = Vec::new();
            let mut force_full = false;

            for path in event.paths {
                if path.starts_with(&index_root) {
                    continue;
                }
                if path
                    .file_name()
                    .is_some_and(|n| n == ".gitignore" || n == ".ignore")
                {
                    force_full = true;
                }
                if path.is_dir() {
                    force_full = true;
                }
                paths.push(path);
            }

            if paths.is_empty() {
                return;
            }
            let _ = tx.send(WatchMsg { paths, force_full });
        })?;

    for root in scan_roots {
        watcher.watch(&root, notify::RecursiveMode::Recursive)?;
    }

    loop {
        let Some(first) = rx.recv().await else {
            return Ok(());
        };

        let mut pending = std::collections::BTreeSet::<PathBuf>::new();
        let mut force_full = first.force_full;
        for p in first.paths {
            pending.insert(p);
        }

        let mut deadline = tokio::time::Instant::now() + debounce;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let sleep = tokio::time::sleep(deadline - now);
            tokio::pin!(sleep);
            tokio::select! {
                _ = &mut sleep => break,
                msg = rx.recv() => {
                    let Some(msg) = msg else { return Ok(()); };
                    force_full |= msg.force_full;
                    for p in msg.paths {
                        pending.insert(p);
                    }
                    deadline = tokio::time::Instant::now() + debounce;
                }
            }
        }

        if force_full || pending.len() > 500 {
            let _ = run_reindex(state.clone(), false).await;
        } else {
            let changed_paths: Vec<PathBuf> = pending.into_iter().collect();
            let _ = run_reindex_changed_paths(state.clone(), &changed_paths).await;
        }
    }
}

async fn run_reindex(state: Arc<AppState>, full: bool) -> anyhow::Result<()> {
    let _guard = state.reindex_lock.lock().await;
    let index = state.index.clone();
    let paths = state.paths.clone();

    tokio::task::spawn_blocking(move || {
        if full {
            index.reindex_full(&paths)
        } else {
            index.reindex(&paths)
        }
    })
    .await??;
    Ok(())
}

async fn run_reindex_changed_paths(
    state: Arc<AppState>,
    changed_paths: &[PathBuf],
) -> anyhow::Result<()> {
    if changed_paths.is_empty() {
        return Ok(());
    }
    let _guard = state.reindex_lock.lock().await;
    let index = state.index.clone();
    let paths = state.paths.clone();
    let changed_paths = changed_paths.to_vec();

    tokio::task::spawn_blocking(move || index.reindex_changed_paths(&paths, &changed_paths))
        .await??;
    Ok(())
}

async fn run_reindex_paths(state: Arc<AppState>, raw_paths: &[String]) -> anyhow::Result<()> {
    let mut paths = Vec::new();
    for raw in raw_paths {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let p = PathBuf::from(raw);
        let p = if p.is_absolute() {
            p
        } else {
            state.paths.project_root.join(p)
        };
        paths.push(p);
    }
    run_reindex_changed_paths(state, &paths).await
}

#[derive(Debug)]
struct WatchMsg {
    paths: Vec<PathBuf>,
    force_full: bool,
}
