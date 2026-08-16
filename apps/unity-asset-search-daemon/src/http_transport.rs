//! Capability-authenticated loopback HTTP transport for the search service.

use std::error::Error as _;
use std::future::{Ready, ready};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONNECTION, CONTENT_TYPE, HOST, ORIGIN, WWW_AUTHENTICATE,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use axum::routing::post;
use axum_server::accept::Accept;
use http_body_util::LengthLimitError;
use hyper_util::rt::TokioTimer;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;
use unity_asset_core::AssetLoadBudget;
use unity_asset_search_local::{HttpCapability, LOOPBACK_HTTP_REQUEST_PATH};
use unity_asset_search_protocol::{
    DaemonInstanceId, MAX_REQUEST_JSON_BYTES, ProjectId, decode_request_json,
};

use crate::lifecycle::BlockingTaskHandle;
use crate::service::SearchService;

const REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_RESPONSE_GRACE: Duration = Duration::from_secs(1);
const MAX_HTTP_CONNECTIONS: usize = 64;
const MAX_HTTP_HEADERS: usize = 32;
const JSON_CONTENT_TYPE: &str = "application/json";
const JSON_UTF8_CONTENT_TYPE: &str = "application/json; charset=utf-8";
const BEARER_PREFIX: &[u8] = b"Bearer ";
const X_CONTENT_TYPE_OPTIONS: HeaderName = HeaderName::from_static("x-content-type-options");

#[derive(Clone)]
struct LoopbackHttpState {
    project_id: ProjectId,
    daemon_instance_id: DaemonInstanceId,
    expected_host: String,
    capability: HttpCapability,
    blocking_tasks: BlockingTaskHandle,
    service: SearchService,
}

/// Bound loopback listener that has not yet started serving or been published for discovery.
pub(crate) struct BoundLoopbackHttp {
    address: SocketAddrV4,
    server: axum_server::Server<SocketAddr, ConnectionLimitAcceptor>,
}

impl BoundLoopbackHttp {
    pub(crate) fn bind() -> Result<Self, LoopbackHttpServerError> {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .map_err(LoopbackHttpServerError::Bind)?;
        listener
            .set_nonblocking(true)
            .map_err(LoopbackHttpServerError::Configure)?;
        let address = match listener
            .local_addr()
            .map_err(LoopbackHttpServerError::Inspect)?
        {
            SocketAddr::V4(address) if *address.ip() == Ipv4Addr::LOCALHOST => address,
            address => return Err(LoopbackHttpServerError::UnexpectedAddress { address }),
        };
        let mut server = axum_server::from_tcp(listener)
            .map_err(LoopbackHttpServerError::Configure)?
            .acceptor(ConnectionLimitAcceptor::new(MAX_HTTP_CONNECTIONS))
            .http1_only();
        server
            .http_builder()
            .http1()
            .timer(TokioTimer::new())
            .header_read_timeout(REQUEST_HEADER_TIMEOUT)
            .keep_alive(false)
            .max_headers(MAX_HTTP_HEADERS);
        Ok(Self { address, server })
    }

    pub(crate) fn into_server(
        self,
        project_id: ProjectId,
        daemon_instance_id: DaemonInstanceId,
        capability: HttpCapability,
        blocking_tasks: BlockingTaskHandle,
        service: SearchService,
    ) -> LoopbackHttpServer {
        let Self { address, server } = self;
        let state = Arc::new(LoopbackHttpState {
            project_id,
            daemon_instance_id,
            expected_host: address.to_string(),
            capability,
            blocking_tasks,
            service,
        });
        let router = Router::new()
            .route(LOOPBACK_HTTP_REQUEST_PATH, post(handle_request))
            .method_not_allowed_fallback(|| async {
                transport_error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed")
            })
            .fallback(|| async { transport_error(StatusCode::NOT_FOUND, "not_found") })
            .with_state(state);
        let handle = axum_server::Handle::new();
        let task = tokio::spawn(
            server
                .handle(handle.clone())
                .serve(router.into_make_service()),
        );
        LoopbackHttpServer {
            address,
            handle,
            task: Some(task),
        }
    }
}

#[derive(Clone)]
struct ConnectionLimitAcceptor {
    capacity: Arc<Semaphore>,
}

impl ConnectionLimitAcceptor {
    fn new(maximum: usize) -> Self {
        Self {
            capacity: Arc::new(Semaphore::new(maximum)),
        }
    }
}

impl<I, S> Accept<I, S> for ConnectionLimitAcceptor {
    type Stream = LimitedConnection<I>;
    type Service = S;
    type Future = Ready<io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        ready(
            Arc::clone(&self.capacity)
                .try_acquire_owned()
                .map(|permit| (LimitedConnection::new(stream, permit), service))
                .map_err(|_| io::Error::other("loopback HTTP connection limit reached")),
        )
    }
}

struct LimitedConnection<I> {
    inner: I,
    _permit: OwnedSemaphorePermit,
}

impl<I> LimitedConnection<I> {
    const fn new(inner: I, permit: OwnedSemaphorePermit) -> Self {
        Self {
            inner,
            _permit: permit,
        }
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for LimitedConnection<I> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(context, buffer)
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for LimitedConnection<I> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_write(context, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(context)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(context)
    }
}

impl std::fmt::Debug for BoundLoopbackHttp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundLoopbackHttp")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

/// One HTTP/1 loopback listener whose requests are delegated to [`SearchService`].
pub(crate) struct LoopbackHttpServer {
    address: SocketAddrV4,
    handle: axum_server::Handle<SocketAddr>,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl LoopbackHttpServer {
    #[must_use]
    pub(crate) const fn address(&self) -> SocketAddrV4 {
        self.address
    }

    pub(crate) async fn wait_for_exit(&mut self) -> Result<(), LoopbackHttpServerError> {
        let result = {
            let task = self
                .task
                .as_mut()
                .ok_or(LoopbackHttpServerError::AlreadyJoined)?;
            task.await
        };
        self.task = None;
        map_server_result(result)
    }

    pub(crate) async fn shutdown_until(
        &mut self,
        mut shutdown: watch::Receiver<Option<Instant>>,
    ) -> Result<(), LoopbackHttpServerError> {
        if self.task.is_none() {
            return Ok(());
        }
        self.handle.graceful_shutdown(None);
        let response_grace_deadline = Instant::now()
            .checked_add(SHUTDOWN_RESPONSE_GRACE)
            .unwrap_or_else(Instant::now);
        loop {
            let requested_deadline =
                (*shutdown.borrow()).ok_or(LoopbackHttpServerError::MissingShutdownDeadline)?;
            // Domain admission closes immediately. The transport keeps a short independent grace
            // so an accepted shutdown request can flush its response even when its requested
            // domain-drain timeout is zero.
            let deadline = requested_deadline.max(response_grace_deadline);
            if deadline <= Instant::now() {
                self.handle.shutdown();
                break;
            }
            tokio::select! {
                result = self.wait_for_exit() => return result,
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        self.handle.shutdown();
                        break;
                    }
                }
                () = tokio::time::sleep_until(deadline) => {
                    self.handle.shutdown();
                    break;
                }
            }
        }
        self.wait_for_exit().await
    }
}

impl Drop for LoopbackHttpServer {
    fn drop(&mut self) {
        self.handle.shutdown();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl std::fmt::Debug for LoopbackHttpServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoopbackHttpServer")
            .field("address", &self.address)
            .field("connections", &self.handle.connection_count())
            .field(
                "task_finished",
                &self.task.as_ref().is_none_or(JoinHandle::is_finished),
            )
            .finish()
    }
}

fn map_server_result(
    result: Result<io::Result<()>, JoinError>,
) -> Result<(), LoopbackHttpServerError> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(source)) => Err(LoopbackHttpServerError::Serve(source)),
        Err(source) => Err(LoopbackHttpServerError::Task(source)),
    }
}

async fn handle_request(
    State(state): State<Arc<LoopbackHttpState>>,
    request: Request<Body>,
) -> Response<Body> {
    if request.uri().query().is_some() {
        return transport_error(StatusCode::BAD_REQUEST, "query_not_allowed");
    }
    if !single_header_matches(request.headers(), HOST, state.expected_host.as_bytes()) {
        return transport_error(StatusCode::BAD_REQUEST, "invalid_host");
    }
    if request.headers().contains_key(ORIGIN) {
        return transport_error(StatusCode::FORBIDDEN, "origin_not_allowed");
    }
    if !authorization_matches(request.headers(), &state.capability) {
        return unauthorized_response();
    }
    if !content_type_is_json(request.headers()) {
        return transport_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "json_required");
    }

    let encoded = match tokio::time::timeout(
        REQUEST_BODY_TIMEOUT,
        to_bytes(request.into_body(), MAX_REQUEST_JSON_BYTES),
    )
    .await
    {
        Err(_) => return transport_error(StatusCode::REQUEST_TIMEOUT, "body_timeout"),
        Ok(Err(error)) if body_limit_exceeded(&error) => {
            return transport_error(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large");
        }
        Ok(Err(_)) => return transport_error(StatusCode::BAD_REQUEST, "invalid_body"),
        Ok(Ok(encoded)) => encoded,
    };
    let project_id = state.project_id;
    let daemon_instance_id = state.daemon_instance_id;
    let query_policy_id = state.service.query_policy_id();
    let decoded = state
        .blocking_tasks
        .run(move || {
            let mut budget = AssetLoadBudget::default();
            let request = decode_request_json(&encoded, &mut budget)
                .map_err(|_| RequestAdmissionError::InvalidRequest)?;
            request
                .bind(project_id, daemon_instance_id, query_policy_id)
                .map_err(|_| RequestAdmissionError::BindingChanged)
        })
        .await;
    let (operation, response_encoder) = match decoded {
        Ok(Ok(bound)) => bound,
        Ok(Err(RequestAdmissionError::InvalidRequest)) => {
            return transport_error(StatusCode::BAD_REQUEST, "invalid_request");
        }
        Ok(Err(RequestAdmissionError::BindingChanged)) => {
            return transport_error(StatusCode::CONFLICT, "binding_changed");
        }
        Err(_) => {
            return transport_error(StatusCode::INTERNAL_SERVER_ERROR, "response_invalid");
        }
    };

    let dispatched = state.service.execute(operation).await;
    let shutdown_after_response = dispatched.shutdown_after_response;
    let encoded = match state
        .blocking_tasks
        .run(move || response_encoder.encode(dispatched.response))
        .await
    {
        Ok(Ok(encoded)) => encoded,
        Ok(Err(_)) | Err(_) => {
            return transport_error(StatusCode::INTERNAL_SERVER_ERROR, "response_invalid");
        }
    };
    if let Some(deadline) = shutdown_after_response {
        state.service.begin_shutdown_at(deadline);
    }
    json_response(StatusCode::OK, Body::from(encoded))
}

enum RequestAdmissionError {
    InvalidRequest,
    BindingChanged,
}

fn authorization_matches(headers: &HeaderMap, expected: &HttpCapability) -> bool {
    let Some(value) = single_header(headers, AUTHORIZATION) else {
        return false;
    };
    let Some(encoded) = value.as_bytes().strip_prefix(BEARER_PREFIX) else {
        return false;
    };
    let Ok(encoded) = std::str::from_utf8(encoded) else {
        return false;
    };
    HttpCapability::from_hex(encoded).is_ok_and(|candidate| expected.matches(&candidate))
}

fn content_type_is_json(headers: &HeaderMap) -> bool {
    single_header(headers, CONTENT_TYPE).is_some_and(|value| {
        value.as_bytes() == JSON_CONTENT_TYPE.as_bytes()
            || value.as_bytes() == JSON_UTF8_CONTENT_TYPE.as_bytes()
    })
}

fn single_header_matches(headers: &HeaderMap, name: HeaderName, expected: &[u8]) -> bool {
    single_header(headers, name).is_some_and(|value| value.as_bytes() == expected)
}

fn single_header(headers: &HeaderMap, name: HeaderName) -> Option<&HeaderValue> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn body_limit_exceeded(error: &axum::Error) -> bool {
    error
        .source()
        .is_some_and(|source| source.is::<LengthLimitError>())
}

fn unauthorized_response() -> Response<Body> {
    let mut response = transport_error(StatusCode::UNAUTHORIZED, "unauthorized");
    response.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"unity-asset-search\""),
    );
    response
}

fn transport_error(status: StatusCode, code: &'static str) -> Response<Body> {
    json_response(status, Body::from(format!("{{\"code\":\"{code}\"}}")))
}

fn json_response(status: StatusCode, body: Body) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(JSON_CONTENT_TYPE));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CONNECTION, HeaderValue::from_static("close"));
    response
}

#[derive(Debug, Error)]
pub(crate) enum LoopbackHttpServerError {
    #[error("failed to bind the loopback HTTP listener: {0}")]
    Bind(#[source] io::Error),
    #[error("failed to configure the loopback HTTP listener: {0}")]
    Configure(#[source] io::Error),
    #[error("failed to inspect the loopback HTTP listener: {0}")]
    Inspect(#[source] io::Error),
    #[error("loopback HTTP listener resolved to unexpected address {address}")]
    UnexpectedAddress { address: SocketAddr },
    #[error("loopback HTTP server failed: {0}")]
    Serve(#[source] io::Error),
    #[error("loopback HTTP server task failed: {0}")]
    Task(#[source] JoinError),
    #[error("loopback HTTP server task has already been joined")]
    AlreadyJoined,
    #[error("loopback HTTP shutdown began without an absolute deadline")]
    MissingShutdownDeadline,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::{Body, to_bytes};
    use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HOST, ORIGIN};
    use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
    use unity_asset_core::AssetLoadBudget;
    use unity_asset_search_index::{IndexPaths, SearchIndex, SearchIndexOptions};
    use unity_asset_search_local::{HttpCapability, generate_daemon_instance_id};
    use unity_asset_search_protocol::{
        BUSINESS_PROTOCOL_REVISION, CapabilitiesRequest, OperationId, ReindexStatusRequest,
        RequestEnvelope, RequestId, RequestOperation, ResponseOperation, ResponseOutcome,
        ShutdownRequest, StatusRequest, decode_response_json, encode_request_json,
    };

    use axum_server::accept::Accept as _;

    use super::{
        BoundLoopbackHttp, ConnectionLimitAcceptor, JSON_CONTENT_TYPE, LOOPBACK_HTTP_REQUEST_PATH,
        LoopbackHttpState, authorization_matches, handle_request,
    };
    use crate::coordinator::{ReindexCoordinatorConfig, ReindexCoordinatorRuntime};
    use crate::lifecycle::{AdmissionGate, BlockingTaskOwner};
    use crate::operations::OperationServiceOwner;
    use crate::service::SearchService;
    use crate::watcher::MaintenanceRuntime;

    fn capability(byte: u8) -> HttpCapability {
        HttpCapability::from_bytes([byte; 32]).unwrap()
    }

    #[tokio::test]
    async fn connection_acceptor_rejects_capacity_overflow_until_a_slot_releases() {
        let acceptor = ConnectionLimitAcceptor::new(1);
        let (first_stream, _) = tokio::io::duplex(64);
        let (second_stream, _) = tokio::io::duplex(64);
        let first = acceptor.accept(first_stream, ()).await.unwrap().0;

        let Err(error) = acceptor.accept(second_stream, ()).await else {
            panic!("connection above the configured capacity was accepted");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::Other);

        drop(first);
        let (third_stream, _) = tokio::io::duplex(64);
        acceptor.accept(third_stream, ()).await.unwrap();
    }

    #[test]
    fn authorization_accepts_only_one_matching_bearer_capability() {
        let expected = capability(7);
        let encoded = expected.encode_hex();
        let encoded = std::str::from_utf8(&encoded).unwrap();
        let mut headers = HeaderMap::new();

        assert!(!authorization_matches(&headers, &expected));

        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {encoded}")).unwrap(),
        );
        assert!(authorization_matches(&headers, &expected));

        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static(
                "Bearer 0808080808080808080808080808080808080808080808080808080808080808",
            ),
        );
        assert!(!authorization_matches(&headers, &expected));

        headers.append(
            AUTHORIZATION,
            HeaderValue::from_static(
                "Bearer 0707070707070707070707070707070707070707070707070707070707070707",
            ),
        );
        assert!(!authorization_matches(&headers, &expected));
    }

    #[tokio::test]
    async fn request_boundary_authenticates_and_binds_before_dispatch() {
        let fixture = ServiceFixture::new();
        let state = fixture.http_state(capability(7), 31337);

        let unauthorized = request(&state, Body::from("{"), false);
        let response = handle_request(axum::extract::State(state.clone()), unauthorized).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let mut origin = request(&state, Body::from("{}"), true);
        origin
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static("https://example.invalid"));
        let response = handle_request(axum::extract::State(state.clone()), origin).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let mut rebound = request(&state, Body::from("{}"), true);
        rebound
            .headers_mut()
            .insert(HOST, HeaderValue::from_static("example.invalid"));
        let response = handle_request(axum::extract::State(state.clone()), rebound).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let authorized = request(&state, Body::from("{"), true);
        let response = handle_request(axum::extract::State(state.clone()), authorized).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let oversized = request(
            &state,
            Body::from(vec![
                b' ';
                unity_asset_search_protocol::MAX_REQUEST_JSON_BYTES + 1
            ]),
            true,
        );
        let response = handle_request(axum::extract::State(state.clone()), oversized).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let stale = RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([9; 16]),
            fixture.project_id,
            generate_daemon_instance_id().unwrap(),
            fixture.service.query_policy_id(),
            RequestOperation::Status(StatusRequest::default()),
        )
        .unwrap();
        let encoded = encode_request_json(&stale).unwrap();
        let response = handle_request(
            axum::extract::State(state.clone()),
            request(&state, Body::from(encoded), true),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);

        let request_envelope = fixture.request(RequestOperation::Capabilities(
            CapabilitiesRequest::default(),
        ));
        let encoded = encode_request_json(&request_envelope).unwrap();
        let response = handle_request(
            axum::extract::State(state.clone()),
            request(&state, Body::from(encoded), true),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            HeaderValue::from_static("no-store")
        );
        let encoded = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let mut budget = AssetLoadBudget::default();
        let response = decode_response_json(&encoded, &mut budget, &request_envelope).unwrap();
        assert!(matches!(
            response.into_outcome(),
            ResponseOutcome::Success(response)
                if matches!(response.as_ref(), ResponseOperation::Capabilities(_))
        ));

        let mut operation_id = [9; 16];
        operation_id[..8].copy_from_slice(&fixture.daemon_instance_id.as_bytes()[..8]);
        let request_envelope =
            fixture.request(RequestOperation::ReindexStatus(ReindexStatusRequest {
                operation_id: OperationId::from_bytes(operation_id),
            }));
        let encoded = encode_request_json(&request_envelope).unwrap();
        let response = handle_request(
            axum::extract::State(state.clone()),
            request(&state, Body::from(encoded), true),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let encoded = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let mut budget = AssetLoadBudget::default();
        let response = decode_response_json(&encoded, &mut budget, &request_envelope).unwrap();
        assert!(matches!(response.into_outcome(), ResponseOutcome::Error(_)));
    }

    #[tokio::test]
    async fn real_loopback_server_accepts_only_the_capability_bound_route() {
        let fixture = ServiceFixture::new();
        let capability = capability(7);
        let bound = BoundLoopbackHttp::bind().unwrap();
        let mut server = bound.into_server(
            fixture.project_id,
            fixture.daemon_instance_id,
            capability.clone(),
            fixture._blocking_tasks.handle(),
            fixture.service.clone(),
        );
        let request_envelope = fixture.request(RequestOperation::Capabilities(
            CapabilitiesRequest::default(),
        ));
        let encoded = encode_request_json(&request_envelope).unwrap();
        let encoded_capability = capability.encode_hex();
        let encoded_capability = std::str::from_utf8(&encoded_capability).unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .http1_only()
            .build()
            .unwrap();
        let response = client
            .post(format!(
                "http://{}{}",
                server.address(),
                LOOPBACK_HTTP_REQUEST_PATH
            ))
            .header(AUTHORIZATION, format!("Bearer {encoded_capability}"))
            .header(CONTENT_TYPE, JSON_CONTENT_TYPE)
            .body(encoded)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let encoded = response.bytes().await.unwrap();
        let mut budget = AssetLoadBudget::default();
        let response = decode_response_json(&encoded, &mut budget, &request_envelope).unwrap();
        assert!(matches!(
            response.into_outcome(),
            ResponseOutcome::Success(response)
                if matches!(response.as_ref(), ResponseOperation::Capabilities(_))
        ));

        let shutdown = fixture.service.subscribe_shutdown();
        fixture
            .service
            .begin_shutdown_at(tokio::time::Instant::now() + Duration::from_secs(1));
        server.shutdown_until(shutdown).await.unwrap();
    }

    #[tokio::test]
    async fn zero_domain_drain_still_flushes_the_accepted_shutdown_response() {
        let fixture = ServiceFixture::new();
        let capability = capability(7);
        let bound = BoundLoopbackHttp::bind().unwrap();
        let mut server = bound.into_server(
            fixture.project_id,
            fixture.daemon_instance_id,
            capability.clone(),
            fixture._blocking_tasks.handle(),
            fixture.service.clone(),
        );
        let request_envelope = fixture.request(RequestOperation::Shutdown(ShutdownRequest {
            drain_timeout_ms: 0,
        }));
        let encoded = encode_request_json(&request_envelope).unwrap();
        let encoded_capability = capability.encode_hex();
        let encoded_capability = std::str::from_utf8(&encoded_capability).unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .http1_only()
            .build()
            .unwrap();
        let response_task = tokio::spawn({
            let address = server.address();
            let encoded_capability = encoded_capability.to_owned();
            async move {
                client
                    .post(format!("http://{address}{LOOPBACK_HTTP_REQUEST_PATH}"))
                    .header(AUTHORIZATION, format!("Bearer {encoded_capability}"))
                    .header(CONTENT_TYPE, JSON_CONTENT_TYPE)
                    .body(encoded)
                    .send()
                    .await
                    .unwrap()
            }
        });

        let mut shutdown = fixture.service.subscribe_shutdown();
        shutdown.wait_for(Option::is_some).await.unwrap();
        server.shutdown_until(shutdown).await.unwrap();

        let response = response_task.await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let encoded = response.bytes().await.unwrap();
        let mut budget = AssetLoadBudget::default();
        let response = decode_response_json(&encoded, &mut budget, &request_envelope).unwrap();
        assert!(matches!(
            response.into_outcome(),
            ResponseOutcome::Success(response)
                if matches!(response.as_ref(), ResponseOperation::Shutdown(shutdown) if shutdown.accepted)
        ));
    }

    fn request(
        state: &LoopbackHttpState,
        body: Body,
        include_authorization: bool,
    ) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri(LOOPBACK_HTTP_REQUEST_PATH)
            .header(HOST, state.expected_host.as_str())
            .header(CONTENT_TYPE, JSON_CONTENT_TYPE)
            .body(body)
            .unwrap();
        if include_authorization {
            let encoded = state.capability.encode_hex();
            let encoded = std::str::from_utf8(&encoded).unwrap();
            request.headers_mut().insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {encoded}")).unwrap(),
            );
        }
        request
    }

    struct ServiceFixture {
        service: SearchService,
        _maintenance: MaintenanceRuntime,
        _operation_service: OperationServiceOwner,
        _coordinator: ReindexCoordinatorRuntime,
        _blocking_tasks: BlockingTaskOwner,
        _project: tempfile::TempDir,
        project_id: unity_asset_search_protocol::ProjectId,
        daemon_instance_id: unity_asset_search_protocol::DaemonInstanceId,
    }

    impl ServiceFixture {
        fn new() -> Self {
            let project = crate::secure_test_tempdir();
            std::fs::create_dir(project.path().join("Assets")).unwrap();
            std::fs::create_dir(project.path().join("ProjectSettings")).unwrap();
            let paths = IndexPaths::for_project(
                project.path().to_path_buf(),
                Some(project.path().join("index")),
                None,
            )
            .unwrap();
            let project_id = paths.project_id();
            let mut budget = AssetLoadBudget::default();
            let index = SearchIndex::open_or_create_with_options(
                paths,
                SearchIndexOptions::default(),
                &mut budget,
            )
            .unwrap();
            let coordinator = ReindexCoordinatorRuntime::start(
                ReindexCoordinatorConfig::new(index.paths().project_path_space().clone())
                    .with_debounce(Duration::from_secs(60))
                    .with_max_debounce(Duration::from_secs(60)),
                |_intent| async move { std::future::pending().await },
            )
            .unwrap();
            let daemon_instance_id = generate_daemon_instance_id().unwrap();
            let blocking_tasks = BlockingTaskOwner::new();
            let admission = AdmissionGate::default();
            let operation_service = OperationServiceOwner::new(
                daemon_instance_id,
                coordinator.coordinator(),
                admission.clone(),
            );
            let maintenance = MaintenanceRuntime::start(operation_service.service(), None, None);
            let query_policy_id = index.status().unwrap().query_policy_id;
            let service = SearchService::new(
                index,
                blocking_tasks.handle(),
                operation_service.service(),
                query_policy_id,
                admission,
                maintenance.handle(),
            );
            Self {
                service,
                _maintenance: maintenance,
                _operation_service: operation_service,
                _coordinator: coordinator,
                _blocking_tasks: blocking_tasks,
                _project: project,
                project_id,
                daemon_instance_id,
            }
        }

        fn http_state(&self, capability: HttpCapability, port: u16) -> Arc<LoopbackHttpState> {
            Arc::new(LoopbackHttpState {
                project_id: self.project_id,
                daemon_instance_id: self.daemon_instance_id,
                expected_host: format!("127.0.0.1:{port}"),
                capability,
                blocking_tasks: self._blocking_tasks.handle(),
                service: self.service.clone(),
            })
        }

        fn request(&self, operation: RequestOperation) -> RequestEnvelope {
            RequestEnvelope::new(
                BUSINESS_PROTOCOL_REVISION,
                RequestId::from_bytes([7; 16]),
                self.project_id,
                self.daemon_instance_id,
                self.service.query_policy_id(),
                operation,
            )
            .unwrap()
        }
    }
}
