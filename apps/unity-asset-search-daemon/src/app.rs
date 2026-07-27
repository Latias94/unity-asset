//! Embeddable HTTP boundary for the search daemon.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Query, RawQuery, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::sync::RwLock;
use tower_http::trace::{DefaultMakeSpan, TraceLayer};

use crate::coordinator::{
    CoordinatorError, FilesystemReindexIntent, FilesystemReindexScope, ReindexCoordinator,
    ReindexSource,
};
use crate::security::{DaemonToken, TokenStore, verify_bearer_token};
use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, ContractJsonLimits, ContractJsonResourceModel,
    read_contract_json_slice,
};
use unity_asset_search_index::{
    ApiError, ApiErrorCode, SearchIndex, SearchIndexError, SearchRequest, StatusResponse,
};
use unity_asset_search_protocol::{
    HEALTH_ENDPOINT, HealthResponse, REFERENCES_ENDPOINT, REINDEX_ENDPOINT, ReindexResponse,
    SEARCH_ENDPOINT, STATUS_ENDPOINT, SUGGEST_ENDPOINT, TOKEN_ROTATE_ENDPOINT,
};
use url::form_urlencoded;

const DEFAULT_SEARCH_LIMIT: usize = 20;
const MAX_SEARCH_LIMIT: usize = 200;
const DEFAULT_SUGGEST_LIMIT: usize = 10;
const MAX_SUGGEST_LIMIT: usize = 50;

const REFERENCES_JSON_MAX_BYTES: usize = 64 * 1024;
// Object selectors permit 64 containment steps; this covers their complete wire tree.
const REFERENCES_JSON_MAX_ENTRIES: u64 = 512;
const REFERENCES_JSON_MAX_MEMBERS: u64 = 512;
const REFERENCES_JSON_MATERIALIZATION_FIXED_BYTES: u64 = 64 * 1024;
const REFERENCES_JSON_MATERIALIZATION_BYTES_PER_ENTRY: u64 = 256;
const REFERENCES_JSON_BUDGET_BYTES: u64 = 1024 * 1024;
// A page can decode two persisted fields for each of 500 hits. Four GiB covers
// their fixed reserves, 8 MiB of stored JSON parser work, and up to two million
// typed structure entries at the larger per-entry materialization rate.
const REFERENCE_QUERY_BUDGET_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const REFERENCE_QUERY_BUDGET_ENTRIES: u64 = 2 * 1024 * 1024;
const REFERENCE_QUERY_BUDGET_MEMBERS: u64 = 2 * 1024 * 1024;
const REFERENCE_QUERY_BUDGET_DEPTH: u32 = 32;

const REINDEX_JSON_MAX_BYTES: usize = 1024 * 1024;
// Bound changed-path cardinality independently from individual path lengths and raw bytes.
const REINDEX_JSON_MAX_ENTRIES: u64 = 32 * 1024;
const REINDEX_JSON_MAX_MEMBERS: u64 = 32 * 1024;
const REINDEX_JSON_MATERIALIZATION_FIXED_BYTES: u64 = 1024 * 1024;
const REINDEX_JSON_MATERIALIZATION_BYTES_PER_ENTRY: u64 = 256;
const REINDEX_JSON_BUDGET_BYTES: u64 = 24 * 1024 * 1024;

const CONTRACT_JSON_MAX_DEPTH: u32 = 32;
const CONTRACT_JSON_PARSER_WORK_MULTIPLIER: u64 = 6;
const CONTRACT_JSON_PARSER_FIXED_WORK_BYTES: u64 = 4 * 1024;

const REFERENCES_JSON_RESOURCES: ContractJsonResourceModel = ContractJsonResourceModel::new(
    CONTRACT_JSON_PARSER_WORK_MULTIPLIER,
    CONTRACT_JSON_PARSER_FIXED_WORK_BYTES,
    REFERENCES_JSON_MATERIALIZATION_FIXED_BYTES,
    REFERENCES_JSON_MATERIALIZATION_BYTES_PER_ENTRY,
);
const REFERENCES_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "daemon.references.request",
    REFERENCES_JSON_MAX_BYTES,
    CONTRACT_JSON_MAX_DEPTH,
    REFERENCES_JSON_MAX_ENTRIES,
    REFERENCES_JSON_MAX_MEMBERS,
    REFERENCES_JSON_RESOURCES,
);
const REINDEX_JSON_RESOURCES: ContractJsonResourceModel = ContractJsonResourceModel::new(
    CONTRACT_JSON_PARSER_WORK_MULTIPLIER,
    CONTRACT_JSON_PARSER_FIXED_WORK_BYTES,
    REINDEX_JSON_MATERIALIZATION_FIXED_BYTES,
    REINDEX_JSON_MATERIALIZATION_BYTES_PER_ENTRY,
);
const REINDEX_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "daemon.reindex.request",
    REINDEX_JSON_MAX_BYTES,
    CONTRACT_JSON_MAX_DEPTH,
    REINDEX_JSON_MAX_ENTRIES,
    REINDEX_JSON_MAX_MEMBERS,
    REINDEX_JSON_RESOURCES,
);

struct AppState {
    index: SearchIndex,
    coordinator: ReindexCoordinator,
    token_store: TokenStore,
    token: RwLock<DaemonToken>,
}

/// Builds an embeddable daemon router for one project and its current credential.
pub fn router(
    index: SearchIndex,
    coordinator: ReindexCoordinator,
    token_store: TokenStore,
    current_token: DaemonToken,
) -> Router {
    let state = Arc::new(AppState {
        index,
        coordinator,
        token_store,
        token: RwLock::new(current_token),
    });

    Router::new()
        .route(HEALTH_ENDPOINT, get(health))
        .route(STATUS_ENDPOINT, get(status))
        .route(SEARCH_ENDPOINT, get(search))
        .route(SUGGEST_ENDPOINT, get(suggest))
        .route(
            REFERENCES_ENDPOINT,
            post(references).layer(DefaultBodyLimit::max(REFERENCES_JSON_MAX_BYTES)),
        )
        .route(
            REINDEX_ENDPOINT,
            post(reindex)
                .layer(DefaultBodyLimit::max(REINDEX_JSON_MAX_BYTES))
                .route_layer(middleware::from_fn_with_state(
                    Arc::clone(&state),
                    authorize_reindex,
                )),
        )
        .route(TOKEN_ROTATE_ENDPOINT, post(rotate_token))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        // Authorization is intentionally omitted from every trace span.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().include_headers(false)),
        )
        .with_state(state)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse::healthy(env!("CARGO_PKG_VERSION")))
}

async fn status(State(state): State<Arc<AppState>>) -> HttpResult<Json<StatusResponse>> {
    let index = state.index.clone();
    blocking_index(move || index.status())
        .await
        .map(status_for_http)
        .map(Json)
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

async fn search(
    State(state): State<Arc<AppState>>,
    query: Result<Query<SearchQuery>, QueryRejection>,
    raw_query: RawQuery,
) -> HttpResult<Json<unity_asset_search_index::SearchResponse>> {
    let Query(query) = parse_query(query)?;
    reject_unknown_query_fields(&raw_query, &["q", "limit"])?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);
    let request = SearchRequest::new(query.q, limit);
    let index = state.index.clone();
    blocking_index(move || index.search(request))
        .await
        .map(Json)
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SuggestQuery {
    #[serde(default)]
    prefix: String,
    limit: Option<usize>,
}

async fn suggest(
    State(state): State<Arc<AppState>>,
    query: Result<Query<SuggestQuery>, QueryRejection>,
    raw_query: RawQuery,
) -> HttpResult<Json<unity_asset_search_index::SuggestResponse>> {
    let Query(query) = parse_query(query)?;
    reject_unknown_query_fields(&raw_query, &["prefix", "limit"])?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_SUGGEST_LIMIT)
        .clamp(1, MAX_SUGGEST_LIMIT);
    let index = state.index.clone();
    blocking_index(move || index.suggest(&query.prefix, limit))
        .await
        .map(Json)
}

async fn references(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> HttpResult<Json<unity_asset_search_index::ReferencesResponse>> {
    require_json_content_type(&headers)?;
    let request = parse_contract_json(
        body,
        REFERENCES_JSON_LIMITS,
        REFERENCES_JSON_BUDGET_BYTES,
        REFERENCES_JSON_MAX_ENTRIES,
        REFERENCES_JSON_MAX_MEMBERS,
    )?;
    let index = state.index.clone();
    let mut budget = reference_query_budget()?;
    blocking_index(move || index.references(request, &mut budget))
        .await
        .map(Json)
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesystemReindexRequest {
    contract_version: u16,
    scope: FilesystemReindexRequestScope,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum FilesystemReindexRequestScope {
    Full {},
    Reconcile {},
    ChangedPaths { paths: Vec<PathBuf> },
}

impl From<FilesystemReindexRequest> for FilesystemReindexIntent {
    fn from(intent: FilesystemReindexRequest) -> Self {
        let scope = match intent.scope {
            FilesystemReindexRequestScope::Full {} => FilesystemReindexScope::Full,
            FilesystemReindexRequestScope::Reconcile {} => FilesystemReindexScope::Reconcile,
            FilesystemReindexRequestScope::ChangedPaths { paths } => {
                FilesystemReindexScope::ChangedPaths { paths }
            }
        };
        Self {
            contract_version: intent.contract_version,
            scope,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReindexQuery {
    wait: Option<bool>,
}

async fn reindex(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    query: Result<Query<ReindexQuery>, QueryRejection>,
    raw_query: RawQuery,
    body: Result<Bytes, BytesRejection>,
) -> HttpResult<Response> {
    require_json_content_type(&headers)?;
    let Query(query) = parse_query(query)?;
    reject_unknown_query_fields(&raw_query, &["wait"])?;
    let intent = parse_contract_json::<FilesystemReindexRequest>(
        body,
        REINDEX_JSON_LIMITS,
        REINDEX_JSON_BUDGET_BYTES,
        REINDEX_JSON_MAX_ENTRIES,
        REINDEX_JSON_MAX_MEMBERS,
    )?
    .into();
    let wait = query.wait.unwrap_or(true);

    if !wait {
        let admission = state
            .coordinator
            .admit(ReindexSource::Http, intent)
            .await
            .map_err(HttpError::from_coordinator)?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(ReindexResponse::accepted(admission)),
        )
            .into_response());
    }

    let completion = state
        .coordinator
        .admit_and_wait(ReindexSource::Http, intent)
        .await
        .map_err(HttpError::from_coordinator)?;
    let index = state.index.clone();
    let actual_status = status_for_http(blocking_index(move || index.status()).await?);
    Ok((
        StatusCode::OK,
        Json(ReindexResponse::waited(
            completion.admission,
            completion.terminal,
            actual_status,
        )),
    )
        .into_response())
}

fn status_for_http(mut status: StatusResponse) -> StatusResponse {
    status.capabilities.change_set_reindex = false;
    status
}

async fn rotate_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> HttpResult<Response> {
    let authorization = headers.get(AUTHORIZATION).cloned();
    let rotation_state = Arc::clone(&state);
    let outcome = tokio::task::spawn_blocking(move || {
        rotate_token_blocking(&rotation_state, authorization.as_ref())
    })
    .await
    .map_err(|_| HttpError::internal("token rotation worker terminated unexpectedly"))?;

    match outcome {
        Ok(()) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(TokenRotationError::Unauthorized) => Err(HttpError::unauthorized()),
        Err(TokenRotationError::Failed) => Err(HttpError::internal("daemon token rotation failed")),
    }
}

fn rotate_token_blocking(
    state: &AppState,
    authorization: Option<&HeaderValue>,
) -> Result<(), TokenRotationError> {
    let mut current = state.token.blocking_write();
    let authorization = authorization.and_then(|value| value.to_str().ok());
    if !verify_bearer_token(authorization, &current) {
        return Err(TokenRotationError::Unauthorized);
    }
    let rotation = state
        .token_store
        .rotate_if_current(&current)
        .map_err(|_| TokenRotationError::Failed)?;
    let (replacement, warning) = rotation.into_parts();
    *current = replacement;
    drop(current);
    if let Some(warning) = warning {
        eprintln!("daemon token rotation warning: {warning}");
    }
    Ok(())
}

enum TokenRotationError {
    Unauthorized,
    Failed,
}

async fn authorize_reindex(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    match require_authorized(request.headers(), &state).await {
        Ok(()) => next.run(request).await,
        Err(error) => error.into_response(),
    }
}

async fn require_authorized(headers: &HeaderMap, state: &AppState) -> HttpResult<()> {
    let token = state.token.read().await;
    let authorization = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if verify_bearer_token(authorization, &token) {
        Ok(())
    } else {
        Err(HttpError::unauthorized())
    }
}

async fn blocking_index<T>(
    operation: impl FnOnce() -> Result<T, SearchIndexError> + Send + 'static,
) -> HttpResult<T>
where
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(operation).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(HttpError::from_api(error.api_error().clone())),
        Err(_) => Err(HttpError::internal(
            "search index worker terminated unexpectedly",
        )),
    }
}

fn parse_query<T>(query: Result<Query<T>, QueryRejection>) -> HttpResult<Query<T>> {
    query.map_err(|_| HttpError::invalid_request("invalid query parameters"))
}

fn require_json_content_type(headers: &HeaderMap) -> HttpResult<()> {
    let is_json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<mime::Mime>().ok())
        .is_some_and(|mime| {
            mime.type_() == mime::APPLICATION
                && (mime.subtype() == mime::JSON || mime.suffix() == Some(mime::JSON))
        });
    if is_json {
        Ok(())
    } else {
        Err(HttpError::invalid_request("invalid JSON request body"))
    }
}

fn parse_contract_json<T: serde::de::DeserializeOwned>(
    body: Result<Bytes, BytesRejection>,
    limits: ContractJsonLimits,
    max_budget_bytes: u64,
    max_entries: u64,
    max_members: u64,
) -> HttpResult<T> {
    let body = body.map_err(|_| HttpError::invalid_request("invalid JSON request body"))?;
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: max_budget_bytes,
        max_depth: CONTRACT_JSON_MAX_DEPTH,
        max_entries,
        max_members,
        ..AssetLoadLimits::default()
    })
    .map_err(|_| HttpError::internal("daemon JSON budget is invalid"))?;
    read_contract_json_slice(&body, &mut budget, limits)
        .map_err(|_| HttpError::invalid_request("invalid JSON request body"))
}

fn reference_query_budget() -> HttpResult<AssetLoadBudget> {
    AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: REFERENCE_QUERY_BUDGET_BYTES,
        max_depth: REFERENCE_QUERY_BUDGET_DEPTH,
        max_entries: REFERENCE_QUERY_BUDGET_ENTRIES,
        max_members: REFERENCE_QUERY_BUDGET_MEMBERS,
        ..AssetLoadLimits::default()
    })
    .map_err(|_| HttpError::internal("reference query budget is invalid"))
}

fn reject_unknown_query_fields(raw_query: &RawQuery, allowed: &[&str]) -> HttpResult<()> {
    if let Some(raw_query) = raw_query.0.as_deref() {
        for (name, _) in form_urlencoded::parse(raw_query.as_bytes()) {
            if !allowed.contains(&name.as_ref()) {
                return Err(HttpError::invalid_request("unknown query parameter"));
            }
        }
    }
    Ok(())
}

async fn not_found() -> HttpError {
    HttpError::at_status(
        StatusCode::NOT_FOUND,
        ApiError::new(
            ApiErrorCode::InvalidRequest,
            "HTTP endpoint was not found",
            false,
        ),
    )
}

async fn method_not_allowed() -> HttpError {
    HttpError::at_status(
        StatusCode::METHOD_NOT_ALLOWED,
        ApiError::new(
            ApiErrorCode::InvalidRequest,
            "HTTP method is not allowed for this endpoint",
            false,
        ),
    )
}

type HttpResult<T> = Result<T, HttpError>;

struct HttpError {
    status: StatusCode,
    error: Box<ApiError>,
}

impl HttpError {
    fn invalid_request(message: &'static str) -> Self {
        Self::from_api(ApiError::new(ApiErrorCode::InvalidRequest, message, false))
    }

    fn unauthorized() -> Self {
        Self::from_api(ApiError::new(
            ApiErrorCode::Unauthorized,
            "bearer authorization is required",
            false,
        ))
    }

    fn internal(message: &'static str) -> Self {
        Self::from_api(ApiError::new(ApiErrorCode::Internal, message, false))
    }

    fn from_coordinator(error: CoordinatorError) -> Self {
        match error {
            CoordinatorError::UnsupportedContractVersion { actual, expected } => Self::from_api(
                ApiError::new(
                    ApiErrorCode::InvalidRequest,
                    "unsupported reindex contract version",
                    false,
                )
                .with_detail("actual", actual.to_string())
                .with_detail("expected", expected.to_string()),
            ),
            CoordinatorError::CompletionWaiterLimit { maximum } => Self::from_api(
                ApiError::new(
                    ApiErrorCode::Busy,
                    "reindex completion waiter limit reached",
                    true,
                )
                .with_detail("maximum", maximum.to_string()),
            ),
            CoordinatorError::CompletionWaiterAllocationFailed => Self::from_api(ApiError::new(
                ApiErrorCode::Busy,
                "could not allocate a reindex completion waiter",
                true,
            )),
            CoordinatorError::ExecutionFailed {
                admission,
                scope,
                message,
            } => Self::from_api(
                ApiError::new(
                    ApiErrorCode::IndexBuildFailed,
                    "reindex execution failed",
                    true,
                )
                .with_detail("admission", format!("{:?}", admission.disposition))
                .with_detail("scope", format!("{scope:?}"))
                .with_detail("cause", message),
            ),
            CoordinatorError::CompletionChannelClosed { admission } => Self::from_api(
                ApiError::new(
                    ApiErrorCode::Internal,
                    "reindex completion channel closed unexpectedly",
                    true,
                )
                .with_detail("admission", format!("{:?}", admission.disposition)),
            ),
            CoordinatorError::PathOutsideProject { .. } => Self::from_api(ApiError::new(
                ApiErrorCode::InvalidRequest,
                "changed path is outside the configured project",
                false,
            )),
            CoordinatorError::InvalidConfiguration(_) => {
                Self::internal("reindex coordinator configuration is invalid")
            }
        }
    }

    fn from_api(error: ApiError) -> Self {
        let status = match error.code {
            ApiErrorCode::InvalidRequest | ApiErrorCode::InvalidCursor => StatusCode::BAD_REQUEST,
            ApiErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiErrorCode::ForbiddenListener => StatusCode::FORBIDDEN,
            ApiErrorCode::Busy | ApiErrorCode::GenerationUnavailable => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ApiErrorCode::RevisionMismatch => StatusCode::CONFLICT,
            ApiErrorCode::IndexBuildFailed | ApiErrorCode::Internal => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        Self {
            status,
            error: Box::new(error),
        }
    }

    fn at_status(status: StatusCode, error: ApiError) -> Self {
        Self {
            status,
            error: Box::new(error),
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.error)).into_response()
    }
}
