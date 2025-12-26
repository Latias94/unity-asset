use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use clap::Parser;
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
}

#[derive(Clone)]
struct AppState {
    index: SearchIndex,
    token: String,
    paths: IndexPaths,
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
    };

    if !args.no_auto_reindex {
        let status = index.status()?;
        if status.indexed_docs == 0 && !status.indexing {
            let index = index.clone();
            let paths = paths.clone();
            tokio::spawn(async move {
                let _ = tokio::task::spawn_blocking(move || index.reindex(&paths)).await;
            });
        }
    }

    let app = axum::Router::new()
        .route("/v1/search", get(search))
        .route("/v1/status", get(status))
        .route("/v1/suggest", get(suggest))
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
    let query = q.q.clone();
    match tokio::task::spawn_blocking(move || index.search(&query, limit)).await {
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

async fn suggest(Query(q): Query<SuggestQuery>) -> impl IntoResponse {
    let _prefix = q.prefix.as_deref().unwrap_or_default();
    let _limit = q.limit.unwrap_or(10);
    (
        StatusCode::OK,
        Json(serde_json::json!({ "suggestions": [] })),
    )
        .into_response()
}

#[derive(Debug, serde::Deserialize)]
struct ReindexParams {
    full: Option<bool>,
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

    let index = state.index.clone();
    let paths = state.paths.clone();
    let full = q.full.unwrap_or(false);
    match tokio::task::spawn_blocking(move || {
        if full {
            index.reindex_full(&paths)
        } else {
            index.reindex(&paths)
        }
    })
    .await
    {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
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
