use std::fs;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Method, Request, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tempfile::TempDir;
use tower::ServiceExt as _;
use tracing_subscriber::fmt::MakeWriter;

use unity_asset_search_daemon::app::router as daemon_router;
use unity_asset_search_daemon::coordinator::{
    ReindexCoordinator, ReindexCoordinatorConfig, ReindexExecution, ReindexSource,
};
use unity_asset_search_daemon::security::{DaemonToken, TokenStore};
use unity_asset_search_index::{
    ApiError, ApiErrorCode, AssetLoadBudget, FilesystemReindexIntent, IndexPaths, ReferenceRequest,
    ReferencesResponse, ReindexDisposition, SEARCH_GENERATION_CONTRACT_VERSION, SearchIndex,
    SearchResponse, StatusResponse, SuggestResponse,
};
use unity_asset_search_protocol::{
    HEALTH_ENDPOINT, HTTP_CONTRACT_VERSION, HealthResponse, REFERENCES_ENDPOINT, REINDEX_ENDPOINT,
    ReindexResponse, SEARCH_ENDPOINT, STATUS_ENDPOINT, SUGGEST_ENDPOINT, TOKEN_ROTATE_ENDPOINT,
    ValidateContractVersion,
};

const RESPONSE_BODY_LIMIT: usize = 4 * 1024 * 1024;
const REFERENCE_GUID: &str = "11112222333344445555666677778888";

struct AppFixture {
    router: Router,
    coordinator: ReindexCoordinator,
    token_store: TokenStore,
    current_token: DaemonToken,
    foreign_token: DaemonToken,
    _temporary: TempDir,
}

impl AppFixture {
    fn new() -> Self {
        Self::with_executor(|index| {
            move |intent| {
                let index = index.clone();
                async move {
                    let result = tokio::task::spawn_blocking(
                        move || -> Result<_, unity_asset_search_index::SearchIndexError> {
                            let receipt = index.reindex(intent, &mut AssetLoadBudget::default())?;
                            let status = index.status()?;
                            Ok(ReindexExecution::new(receipt, status))
                        },
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("test reindex worker terminated"))?;
                    result.map_err(anyhow::Error::new)
                }
            }
        })
    }

    fn with_executor<Factory, Executor, BuildFuture>(factory: Factory) -> Self
    where
        Factory: FnOnce(SearchIndex) -> Executor,
        Executor: Fn(FilesystemReindexIntent) -> BuildFuture + Send + Sync + 'static,
        BuildFuture: Future<Output = anyhow::Result<ReindexExecution>> + Send + 'static,
    {
        Self::with_executor_and_debounce(factory, Duration::from_millis(1))
    }

    fn with_executor_and_debounce<Factory, Executor, BuildFuture>(
        factory: Factory,
        debounce: Duration,
    ) -> Self
    where
        Factory: FnOnce(SearchIndex) -> Executor,
        Executor: Fn(FilesystemReindexIntent) -> BuildFuture + Send + Sync + 'static,
        BuildFuture: Future<Output = anyhow::Result<ReindexExecution>> + Send + 'static,
    {
        let temporary = TempDir::new().expect("temporary directory must be creatable");
        let project_root = temporary.path().join("project");
        fs::create_dir_all(project_root.join("Assets"))
            .expect("test project Assets directory must be creatable");

        let paths = IndexPaths::for_project(
            project_root.clone(),
            Some(temporary.path().join("index")),
            Some(vec!["Assets".into()]),
        )
        .expect("test index paths must be valid");
        let index = SearchIndex::open_or_create(paths.clone(), &mut AssetLoadBudget::default())
            .expect("test index must open");
        index
            .reindex(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .expect("initial test generation must build");

        let token_store =
            TokenStore::open(paths.index_root()).expect("project token store must open");
        let current_token = token_store.create().expect("project token must be created");

        let foreign_index_root = temporary.path().join("foreign-project").join("index");
        fs::create_dir_all(&foreign_index_root)
            .expect("foreign project index directory must be creatable");
        let foreign_token = TokenStore::open(&foreign_index_root)
            .expect("foreign project token store must open")
            .create()
            .expect("foreign project token must be created");

        let coordinator = ReindexCoordinator::new(
            ReindexCoordinatorConfig::new(project_root)
                .with_debounce(debounce)
                .with_max_debounce(debounce),
            factory(index.clone()),
        )
        .expect("test coordinator must be constructible");
        let router = daemon_router(
            index,
            coordinator.clone(),
            token_store.clone(),
            current_token.clone(),
        );

        Self {
            router,
            coordinator,
            token_store,
            current_token,
            foreign_token,
            _temporary: temporary,
        }
    }
}

struct CapturedResponse {
    status: StatusCode,
    body: Vec<u8>,
}

impl CapturedResponse {
    fn json<T: DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).expect("response body must satisfy its public contract")
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.body).expect("HTTP response body must be UTF-8")
    }
}

async fn dispatch(router: &Router, request: Request<Body>) -> CapturedResponse {
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router service must be infallible");
    let status = response.status();
    let body = to_bytes(response.into_body(), RESPONSE_BODY_LIMIT)
        .await
        .expect("response body must fit the contract test limit")
        .to_vec();
    CapturedResponse { status, body }
}

fn empty_request(method: Method, uri: &str, token: Option<&DaemonToken>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(AUTHORIZATION, bearer(token));
    }
    builder
        .body(Body::empty())
        .expect("test request must be valid")
}

fn json_request(uri: &str, value: &impl Serialize, token: Option<&DaemonToken>) -> Request<Body> {
    raw_json_request(
        uri,
        serde_json::to_vec(value).expect("test request contract must serialize"),
        token,
    )
}

fn raw_json_request(
    uri: &str,
    body: impl Into<Body>,
    token: Option<&DaemonToken>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(AUTHORIZATION, bearer(token));
    }
    builder
        .body(body.into())
        .expect("test request must be valid")
}

fn bearer(token: &DaemonToken) -> String {
    format!("Bearer {}", token.expose_secret())
}

fn assert_api_error(
    response: &CapturedResponse,
    expected_status: StatusCode,
    expected_code: ApiErrorCode,
) -> ApiError {
    assert_eq!(response.status, expected_status);
    let error = response.json::<ApiError>();
    assert_eq!(error.contract_version, SEARCH_GENERATION_CONTRACT_VERSION);
    assert_eq!(error.code, expected_code);
    error
        .validate_contract_version()
        .expect("API error version must be supported");
    error
}

#[tokio::test(flavor = "current_thread")]
async fn read_routes_are_unauthenticated_and_use_public_response_contracts() {
    let fixture = AppFixture::new();

    let health = dispatch(
        &fixture.router,
        empty_request(Method::GET, HEALTH_ENDPOINT, None),
    )
    .await;
    assert_eq!(health.status, StatusCode::OK);
    let health = health.json::<HealthResponse>();
    assert!(health.ok);
    assert_eq!(health.contract_version, HTTP_CONTRACT_VERSION);
    health
        .validate_contract_version()
        .expect("health response version must be supported");

    let status = dispatch(
        &fixture.router,
        empty_request(Method::GET, STATUS_ENDPOINT, None),
    )
    .await;
    assert_eq!(status.status, StatusCode::OK);
    let status = status.json::<StatusResponse>();
    assert_eq!(status.contract_version, SEARCH_GENERATION_CONTRACT_VERSION);
    assert!(status.generation.active.is_some());
    assert!(!status.capabilities.change_set_reindex);
    status
        .validate_contract_version()
        .expect("status response version must be supported");

    let search = dispatch(
        &fixture.router,
        empty_request(
            Method::GET,
            &format!("{SEARCH_ENDPOINT}?q=hero&limit=3"),
            None,
        ),
    )
    .await;
    assert_eq!(search.status, StatusCode::OK);
    let search = search.json::<SearchResponse>();
    assert_eq!(search.contract_version, SEARCH_GENERATION_CONTRACT_VERSION);
    assert_eq!(search.query, "hero");

    let suggest = dispatch(
        &fixture.router,
        empty_request(
            Method::GET,
            &format!("{SUGGEST_ENDPOINT}?prefix=he&limit=3"),
            None,
        ),
    )
    .await;
    assert_eq!(suggest.status, StatusCode::OK);
    let suggest = suggest.json::<SuggestResponse>();
    assert_eq!(suggest.contract_version, SEARCH_GENERATION_CONTRACT_VERSION);
    assert_eq!(suggest.prefix, "he");

    let request = ReferenceRequest::incoming_guid(REFERENCE_GUID, None, 8);
    let references = dispatch(
        &fixture.router,
        json_request(REFERENCES_ENDPOINT, &request, None),
    )
    .await;
    assert_eq!(references.status, StatusCode::OK);
    let references = references.json::<ReferencesResponse>();
    assert_eq!(
        references.contract_version,
        SEARCH_GENERATION_CONTRACT_VERSION
    );
    assert_eq!(references.request, request);
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_and_versioned_requests_return_typed_api_errors() {
    let fixture = AppFixture::new();

    let request = ReferenceRequest::incoming_guid(REFERENCE_GUID, None, 8);
    let mut unknown_field =
        serde_json::to_value(&request).expect("reference request must serialize");
    unknown_field
        .as_object_mut()
        .expect("reference request must serialize as an object")
        .insert("unknown_field".to_owned(), true.into());
    let response = dispatch(
        &fixture.router,
        json_request(REFERENCES_ENDPOINT, &unknown_field, None),
    )
    .await;
    assert_api_error(
        &response,
        StatusCode::BAD_REQUEST,
        ApiErrorCode::InvalidRequest,
    );

    let mut nested_reference =
        serde_json::to_value(&request).expect("reference request must serialize");
    nested_reference["selector"]["unknown_nested"] = true.into();
    let response = dispatch(
        &fixture.router,
        json_request(REFERENCES_ENDPOINT, &nested_reference, None),
    )
    .await;
    assert_api_error(
        &response,
        StatusCode::BAD_REQUEST,
        ApiErrorCode::InvalidRequest,
    );

    let query = dispatch(
        &fixture.router,
        empty_request(
            Method::GET,
            &format!("{SEARCH_ENDPOINT}?q=hero&unknown=true"),
            None,
        ),
    )
    .await;
    assert_api_error(
        &query,
        StatusCode::BAD_REQUEST,
        ApiErrorCode::InvalidRequest,
    );

    for unsupported_version in [
        SEARCH_GENERATION_CONTRACT_VERSION - 1,
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
    ] {
        let mut reference_version =
            serde_json::to_value(&request).expect("reference request must serialize");
        reference_version["contract_version"] = unsupported_version.into();
        let response = dispatch(
            &fixture.router,
            json_request(REFERENCES_ENDPOINT, &reference_version, None),
        )
        .await;
        assert_api_error(
            &response,
            StatusCode::BAD_REQUEST,
            ApiErrorCode::InvalidRequest,
        );

        let mut intent = serde_json::to_value(FilesystemReindexIntent::reconcile())
            .expect("reindex intent must serialize");
        intent["contract_version"] = unsupported_version.into();
        let response = dispatch(
            &fixture.router,
            json_request(REINDEX_ENDPOINT, &intent, Some(&fixture.current_token)),
        )
        .await;
        let error = assert_api_error(
            &response,
            StatusCode::BAD_REQUEST,
            ApiErrorCode::InvalidRequest,
        );
        let actual_version = unsupported_version.to_string();
        let expected_version = SEARCH_GENERATION_CONTRACT_VERSION.to_string();
        assert_eq!(
            error.details.get("actual").map(String::as_str),
            Some(actual_version.as_str())
        );
        assert_eq!(
            error.details.get("expected").map(String::as_str),
            Some(expected_version.as_str())
        );
    }

    let response = dispatch(
        &fixture.router,
        json_request(
            REINDEX_ENDPOINT,
            &change_set_request(),
            Some(&fixture.current_token),
        ),
    )
    .await;
    let error = assert_api_error(
        &response,
        StatusCode::BAD_REQUEST,
        ApiErrorCode::InvalidRequest,
    );
    assert_eq!(error.message, "invalid JSON request body");

    let mut nested_intent = serde_json::to_value(FilesystemReindexIntent::reconcile())
        .expect("reindex intent must serialize");
    nested_intent["scope"]["unknown_nested"] = true.into();
    let response = dispatch(
        &fixture.router,
        json_request(
            REINDEX_ENDPOINT,
            &nested_intent,
            Some(&fixture.current_token),
        ),
    )
    .await;
    assert_api_error(
        &response,
        StatusCode::BAD_REQUEST,
        ApiErrorCode::InvalidRequest,
    );
}

#[tokio::test(flavor = "current_thread")]
async fn request_json_boundaries_reject_raw_depth_width_and_trailing_input() {
    let fixture = AppFixture::new();

    let oversized_references = dispatch(
        &fixture.router,
        raw_json_request(REFERENCES_ENDPOINT, vec![b' '; 64 * 1024 + 1], None),
    )
    .await;
    assert_api_error(
        &oversized_references,
        StatusCode::BAD_REQUEST,
        ApiErrorCode::InvalidRequest,
    );

    let deeply_nested = format!("{{\"unknown\":{}0{}}}", "[".repeat(33), "]".repeat(33));
    let depth = dispatch(
        &fixture.router,
        raw_json_request(REFERENCES_ENDPOINT, deeply_nested, None),
    )
    .await;
    assert_api_error(
        &depth,
        StatusCode::BAD_REQUEST,
        ApiErrorCode::InvalidRequest,
    );

    let wide = serde_json::json!({ "unknown": vec![0_u8; 513] });
    let width = dispatch(
        &fixture.router,
        json_request(REFERENCES_ENDPOINT, &wide, None),
    )
    .await;
    assert_api_error(
        &width,
        StatusCode::BAD_REQUEST,
        ApiErrorCode::InvalidRequest,
    );

    let valid = serde_json::to_string(&ReferenceRequest::incoming_guid(REFERENCE_GUID, None, 8))
        .expect("reference request must serialize");
    let trailing = dispatch(
        &fixture.router,
        raw_json_request(REFERENCES_ENDPOINT, format!("{valid} null"), None),
    )
    .await;
    assert_api_error(
        &trailing,
        StatusCode::BAD_REQUEST,
        ApiErrorCode::InvalidRequest,
    );

    let oversized_reindex = dispatch(
        &fixture.router,
        raw_json_request(
            REINDEX_ENDPOINT,
            vec![b' '; 1024 * 1024 + 1],
            Some(&fixture.current_token),
        ),
    )
    .await;
    assert_api_error(
        &oversized_reindex,
        StatusCode::BAD_REQUEST,
        ApiErrorCode::InvalidRequest,
    );

    let reindex_depth = format!(
        "{{\"contract_version\":{SEARCH_GENERATION_CONTRACT_VERSION},\"scope\":{{\"kind\":\"reconcile\",\"unknown\":{}0{}}}}}",
        "[".repeat(33),
        "]".repeat(33),
    );
    let depth = dispatch(
        &fixture.router,
        raw_json_request(
            REINDEX_ENDPOINT,
            reindex_depth,
            Some(&fixture.current_token),
        ),
    )
    .await;
    assert_api_error(
        &depth,
        StatusCode::BAD_REQUEST,
        ApiErrorCode::InvalidRequest,
    );

    let wide_reindex = serde_json::json!({
        "contract_version": SEARCH_GENERATION_CONTRACT_VERSION,
        "scope": {
            "kind": "changed_paths",
            "paths": vec![""; 32 * 1024],
        },
    });
    let width = dispatch(
        &fixture.router,
        json_request(
            REINDEX_ENDPOINT,
            &wide_reindex,
            Some(&fixture.current_token),
        ),
    )
    .await;
    assert_api_error(
        &width,
        StatusCode::BAD_REQUEST,
        ApiErrorCode::InvalidRequest,
    );

    let valid_reindex = serde_json::to_string(&FilesystemReindexIntent::reconcile())
        .expect("reindex intent must serialize");
    let trailing = dispatch(
        &fixture.router,
        raw_json_request(
            REINDEX_ENDPOINT,
            format!("{valid_reindex} null"),
            Some(&fixture.current_token),
        ),
    )
    .await;
    assert_api_error(
        &trailing,
        StatusCode::BAD_REQUEST,
        ApiErrorCode::InvalidRequest,
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reindex_wait_modes_report_admission_and_the_real_terminal_generation() {
    let fixture = AppFixture::new();

    let accepted = dispatch(
        &fixture.router,
        json_request(
            &format!("{REINDEX_ENDPOINT}?wait=false"),
            &FilesystemReindexIntent::reconcile(),
            Some(&fixture.current_token),
        ),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED);
    let accepted = accepted.json::<ReindexResponse>();
    accepted
        .validate_contract_version()
        .expect("accepted response version must be supported");
    assert_eq!(accepted.admission.disposition, ReindexDisposition::Queued);
    assert!(accepted.completion.is_none());
    assert!(accepted.status.is_none());

    let waited = dispatch(
        &fixture.router,
        json_request(
            REINDEX_ENDPOINT,
            &FilesystemReindexIntent::reconcile(),
            Some(&fixture.current_token),
        ),
    )
    .await;
    assert_eq!(waited.status, StatusCode::OK);
    let waited = waited.json::<ReindexResponse>();
    waited
        .validate_contract_version()
        .expect("waited response version must be supported");
    let completion = waited
        .completion
        .as_ref()
        .expect("waited response must contain a terminal receipt");
    let status = waited
        .status
        .as_ref()
        .expect("waited response must contain post-build status");
    assert_eq!(completion.disposition, ReindexDisposition::Applied);
    assert_eq!(
        completion.generation.as_ref(),
        status.generation.active.as_ref()
    );
    assert!(!status.capabilities.change_set_reindex);
}

#[tokio::test(flavor = "current_thread")]
async fn startup_watcher_timer_and_http_share_one_real_admission_window() {
    let runs = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let fixture = AppFixture::with_executor_and_debounce(
        {
            let runs = Arc::clone(&runs);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            move |index| {
                move |intent| {
                    let index = index.clone();
                    let runs = Arc::clone(&runs);
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    async move {
                        if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                            started.notify_one();
                            release.notified().await;
                        }
                        let result = tokio::task::spawn_blocking(
                            move || -> Result<_, unity_asset_search_index::SearchIndexError> {
                                let receipt =
                                    index.reindex(intent, &mut AssetLoadBudget::default())?;
                                let status = index.status()?;
                                Ok(ReindexExecution::new(receipt, status))
                            },
                        )
                        .await
                        .map_err(|_| anyhow::anyhow!("test reindex worker terminated"))?;
                        result.map_err(anyhow::Error::new)
                    }
                }
            }
        },
        Duration::from_millis(100),
    );

    let router = fixture.router.clone();
    let token = fixture.current_token.clone();
    let http = tokio::spawn(async move {
        dispatch(
            &router,
            json_request(
                REINDEX_ENDPOINT,
                &FilesystemReindexIntent::reconcile(),
                Some(&token),
            ),
        )
        .await
    });
    tokio::task::yield_now().await;

    fixture
        .coordinator
        .admit(ReindexSource::Startup, FilesystemReindexIntent::reconcile())
        .await
        .unwrap();
    fixture
        .coordinator
        .admit(ReindexSource::Watcher, FilesystemReindexIntent::reconcile())
        .await
        .unwrap();
    fixture
        .coordinator
        .admit(ReindexSource::Timer, FilesystemReindexIntent::reconcile())
        .await
        .unwrap();

    started.notified().await;
    release.notify_one();
    let response = http.await.unwrap();
    assert_eq!(response.status, StatusCode::OK);
    fixture.coordinator.wait_for_idle().await;

    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let snapshot = fixture.coordinator.snapshot().await;
    assert_eq!(snapshot.admissions.startup, 1);
    assert_eq!(snapshot.admissions.watcher, 1);
    assert_eq!(snapshot.admissions.timer, 1);
    assert_eq!(snapshot.admissions.http, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn waited_reindex_maps_executor_errors_panics_and_cancellation_to_typed_failures() {
    let failed = AppFixture::with_executor(|_| {
        |_intent| async { Err(anyhow::anyhow!("injected executor failure")) }
    });
    let response = dispatch(
        &failed.router,
        json_request(
            REINDEX_ENDPOINT,
            &FilesystemReindexIntent::full(),
            Some(&failed.current_token),
        ),
    )
    .await;
    let error = assert_api_error(
        &response,
        StatusCode::INTERNAL_SERVER_ERROR,
        ApiErrorCode::IndexBuildFailed,
    );
    assert_eq!(error.message, "reindex execution failed");
    assert!(
        error
            .details
            .get("cause")
            .is_some_and(|cause| cause.contains("injected executor failure"))
    );

    let panicked = AppFixture::with_executor(|_| {
        |_intent| -> std::future::Ready<anyhow::Result<ReindexExecution>> {
            panic!("injected synchronous executor panic")
        }
    });
    let response = dispatch(
        &panicked.router,
        json_request(
            REINDEX_ENDPOINT,
            &FilesystemReindexIntent::reconcile(),
            Some(&panicked.current_token),
        ),
    )
    .await;
    let error = assert_api_error(
        &response,
        StatusCode::INTERNAL_SERVER_ERROR,
        ApiErrorCode::IndexBuildFailed,
    );
    assert!(
        error
            .details
            .get("cause")
            .is_some_and(|cause| cause.contains("panicked before returning"))
    );

    let cancelled = AppFixture::with_executor(|_| {
        |_intent| async {
            let task = tokio::spawn(async {
                std::future::pending::<()>().await;
            });
            task.abort();
            task.await.map_err(anyhow::Error::new)?;
            Err(anyhow::anyhow!(
                "cancelled executor child unexpectedly completed"
            ))
        }
    });
    let response = dispatch(
        &cancelled.router,
        json_request(
            REINDEX_ENDPOINT,
            &FilesystemReindexIntent::reconcile(),
            Some(&cancelled.current_token),
        ),
    )
    .await;
    let error = assert_api_error(
        &response,
        StatusCode::INTERNAL_SERVER_ERROR,
        ApiErrorCode::IndexBuildFailed,
    );
    assert!(
        error
            .details
            .get("cause")
            .is_some_and(|cause| cause.contains("cancel"))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn every_endpoint_returns_typed_method_errors_and_unknown_paths_return_typed_not_found() {
    let fixture = AppFixture::new();
    for (endpoint, wrong_method) in [
        (HEALTH_ENDPOINT, Method::POST),
        (STATUS_ENDPOINT, Method::POST),
        (SEARCH_ENDPOINT, Method::POST),
        (SUGGEST_ENDPOINT, Method::POST),
        (REFERENCES_ENDPOINT, Method::GET),
        (REINDEX_ENDPOINT, Method::GET),
        (TOKEN_ROTATE_ENDPOINT, Method::GET),
    ] {
        let response = dispatch(&fixture.router, empty_request(wrong_method, endpoint, None)).await;
        let error = assert_api_error(
            &response,
            StatusCode::METHOD_NOT_ALLOWED,
            ApiErrorCode::InvalidRequest,
        );
        assert_eq!(
            error.message,
            "HTTP method is not allowed for this endpoint"
        );
    }

    for unknown in ["/v3/unknown", "/v2/health", "/v1/health"] {
        let response = dispatch(&fixture.router, empty_request(Method::GET, unknown, None)).await;
        let error = assert_api_error(
            &response,
            StatusCode::NOT_FOUND,
            ApiErrorCode::InvalidRequest,
        );
        assert_eq!(error.message, "HTTP endpoint was not found");
    }
}

fn change_set_request() -> serde_json::Value {
    let digest = format!("blake3-v1:{}", "11".repeat(32));
    let from_revision = format!("blake3-v1:{}", "22".repeat(32));
    let to_revision = format!("blake3-v1:{}", "33".repeat(32));
    serde_json::json!({
        "contract_version": SEARCH_GENERATION_CONTRACT_VERSION,
        "scope": {
            "kind": "change_set",
            "changes": {
                "version": 1,
                "transaction": digest,
                "workspace": "workspace-v1:00000000000000000000000000000001",
                "from_revision": from_revision,
                "to_revision": to_revision,
                "changed_sources": [{
                    "version": 1,
                    "workspace": "workspace-v1:00000000000000000000000000000001",
                    "kind": "serialized_file",
                    "local": "00000000000000000000000000000001",
                }],
                "changed_objects": [],
                "identity_remaps": [],
            },
        },
    })
}

#[derive(Debug, Clone, Copy)]
enum MutatingRoute {
    Reindex,
    RotateToken,
}

impl MutatingRoute {
    const ALL: [Self; 2] = [Self::Reindex, Self::RotateToken];

    fn request(self, token: Option<&DaemonToken>) -> Request<Body> {
        match self {
            Self::Reindex => json_request(
                REINDEX_ENDPOINT,
                &FilesystemReindexIntent::reconcile(),
                token,
            ),
            Self::RotateToken => empty_request(Method::POST, TOKEN_ROTATE_ENDPOINT, token),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn mutations_require_the_current_project_token_and_rotation_leaks_no_secret() {
    let fixture = AppFixture::new();
    let wrong_token = DaemonToken::generate().expect("wrong token must be generated");
    let trace_capture = TraceCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_writer(trace_capture.clone())
        .finish();
    let _trace_guard = tracing::subscriber::set_default(subscriber);
    let mut responses = Vec::new();

    for token in [None, Some(&wrong_token), Some(&fixture.foreign_token)] {
        for route in MutatingRoute::ALL {
            let response = dispatch(&fixture.router, route.request(token)).await;
            assert_api_error(
                &response,
                StatusCode::UNAUTHORIZED,
                ApiErrorCode::Unauthorized,
            );
            responses.push(response);
        }
    }

    let malformed_unauthenticated = Request::builder()
        .method(Method::POST)
        .uri(REINDEX_ENDPOINT)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from("{"))
        .expect("malformed test request must still be constructible");
    let response = dispatch(&fixture.router, malformed_unauthenticated).await;
    assert_api_error(
        &response,
        StatusCode::UNAUTHORIZED,
        ApiErrorCode::Unauthorized,
    );
    responses.push(response);

    let oversized_unauthenticated =
        raw_json_request(REINDEX_ENDPOINT, vec![b' '; 1024 * 1024 + 1], None);
    let response = dispatch(&fixture.router, oversized_unauthenticated).await;
    assert_api_error(
        &response,
        StatusCode::UNAUTHORIZED,
        ApiErrorCode::Unauthorized,
    );
    responses.push(response);

    let response = dispatch(
        &fixture.router,
        MutatingRoute::Reindex.request(Some(&fixture.current_token)),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let reindex = response.json::<ReindexResponse>();
    assert_eq!(
        reindex.admission.contract_version,
        SEARCH_GENERATION_CONTRACT_VERSION
    );
    assert!(reindex.completion.is_some());
    assert!(reindex.status.is_some());
    assert!(
        !reindex
            .status
            .as_ref()
            .expect("waited response must contain status")
            .capabilities
            .change_set_reindex
    );
    reindex
        .validate_contract_version()
        .expect("reindex response version must be supported");
    responses.push(response);

    let response = dispatch(
        &fixture.router,
        MutatingRoute::RotateToken.request(Some(&fixture.current_token)),
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);
    assert!(response.body.is_empty());
    responses.push(response);

    let replacement = fixture
        .token_store
        .load()
        .expect("replacement token must be persisted before rotation returns");
    assert_ne!(
        replacement.expose_secret(),
        fixture.current_token.expose_secret()
    );

    for route in MutatingRoute::ALL {
        let response = dispatch(&fixture.router, route.request(Some(&fixture.current_token))).await;
        assert_api_error(
            &response,
            StatusCode::UNAUTHORIZED,
            ApiErrorCode::Unauthorized,
        );
        responses.push(response);
    }

    let response = dispatch(
        &fixture.router,
        MutatingRoute::Reindex.request(Some(&replacement)),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let reindex = response.json::<ReindexResponse>();
    assert!(reindex.completion.is_some());
    assert!(reindex.status.is_some());
    reindex
        .validate_contract_version()
        .expect("reindex response version must be supported");
    responses.push(response);

    let secrets = [
        &fixture.current_token,
        &replacement,
        &wrong_token,
        &fixture.foreign_token,
    ];
    for response in &responses {
        assert_no_secrets(response.text(), &secrets);
    }
    let trace_output = trace_capture.output();
    assert!(
        !trace_output.is_empty(),
        "trace capture must observe the configured HTTP layer"
    );
    assert_no_secrets(&trace_output, &secrets);
}

fn assert_no_secrets(output: &str, secrets: &[&DaemonToken]) {
    assert!(
        secrets
            .iter()
            .all(|token| !output.contains(token.expose_secret())),
        "credential material leaked across the HTTP boundary"
    );
}

#[derive(Clone, Default)]
struct TraceCapture {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl TraceCapture {
    fn output(&self) -> String {
        String::from_utf8(
            self.bytes
                .lock()
                .expect("trace capture lock must not be poisoned")
                .clone(),
        )
        .expect("trace output must be UTF-8")
    }
}

struct TraceWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl io::Write for TraceWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes
            .lock()
            .map_err(|_| io::Error::other("trace capture lock poisoned"))?
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> MakeWriter<'writer> for TraceCapture {
    type Writer = TraceWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        TraceWriter {
            bytes: Arc::clone(&self.bytes),
        }
    }
}
