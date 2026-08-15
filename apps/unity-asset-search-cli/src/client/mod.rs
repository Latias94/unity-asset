use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rand::TryRngCore as _;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONNECTION, CONTENT_TYPE, HOST, HeaderMap, HeaderValue,
};
use reqwest::{Client, StatusCode};
use unity_asset_core::AssetLoadBudget;
use unity_asset_search_local::{
    DiscoveredLoopbackEndpoint, EndpointNamespaceV1, EndpointStoreError, HTTP_CAPABILITY_HEX_BYTES,
    LOOPBACK_HTTP_REQUEST_PATH, LoopbackEndpointDescriptorError, PrivateRootsV1, ProjectLocatorV1,
};
use unity_asset_search_protocol::{
    CapabilitiesRequest, DaemonInstanceId, ProjectId, QueryPolicyId, RequestEnvelope, RequestId,
    RequestOperation, ResponseOperation, ResponseOutcome, decode_response_json,
    encode_request_json, max_response_json_bytes,
};

use crate::command::{Args, DaemonStartSettings};
use crate::output::CliFailure;

const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(50);
const MAX_START_RETRY_DELAY: Duration = Duration::from_secs(1);
const SERVER_WAIT_RESPONSE_MARGIN: Duration = Duration::from_secs(2);
const JSON_CONTENT_TYPE: &str = "application/json";
const BEARER_PREFIX: &[u8] = b"Bearer ";

#[derive(Debug, Clone)]
pub struct ConnectionOptions {
    project_root: PathBuf,
    index_dir: Option<PathBuf>,
    daemon_binary: Option<PathBuf>,
    connect_timeout: Duration,
    request_timeout: Duration,
    http: Client,
}

impl ConnectionOptions {
    pub fn from_args(args: &Args) -> Result<Self, CliFailure> {
        let connect_timeout = args.connect_timeout();
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect_timeout)
            .http1_only()
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|error| {
                CliFailure::internal(format!("configure loopback HTTP client: {error}"))
            })?;
        Ok(Self {
            project_root: args.project_root()?,
            index_dir: args.index_dir(),
            daemon_binary: args.daemon_binary(),
            connect_timeout,
            request_timeout: args.request_timeout(),
            http,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EndpointBinding {
    pub project_id: ProjectId,
    pub daemon_instance_id: DaemonInstanceId,
    pub query_policy_id: QueryPolicyId,
}

struct CompletedRequest {
    binding: EndpointBinding,
    outcome: ResponseOutcome,
    server_pid: u32,
}

struct ConnectionContext {
    project: ProjectLocatorV1,
    roots: PrivateRootsV1,
    namespace: EndpointNamespaceV1,
}

impl ConnectionContext {
    fn open(options: &ConnectionOptions) -> Result<Self, ConnectError> {
        let project =
            ProjectLocatorV1::open(&options.project_root).map_err(ConnectError::Project)?;
        let roots = PrivateRootsV1::discover_for_current_context().map_err(ConnectError::Roots)?;
        let namespace = roots
            .runtime()
            .endpoint_namespace(project.project_id())
            .map_err(ConnectError::Roots)?;
        Ok(Self {
            project,
            roots,
            namespace,
        })
    }

    fn discover_endpoint(&self) -> Result<DiscoveredLoopbackEndpoint, ConnectError> {
        self.project.revalidate().map_err(ConnectError::Project)?;
        self.roots.revalidate().map_err(ConnectError::Roots)?;
        self.namespace.revalidate().map_err(ConnectError::Roots)?;
        self.namespace
            .discover_loopback_endpoint()
            .map_err(ConnectError::Store)
    }
}

pub async fn execute(
    options: &ConnectionOptions,
    start: Option<&DaemonStartSettings>,
    operation: RequestOperation,
) -> Result<(EndpointBinding, Box<ResponseOperation>), CliFailure> {
    let completed = request_with_start(options, start, operation).await?;
    match completed.outcome {
        ResponseOutcome::Success(response) => Ok((completed.binding, response)),
        ResponseOutcome::Error(error) => Err(CliFailure::daemon(*error)),
    }
}

async fn request_with_start(
    options: &ConnectionOptions,
    start: Option<&DaemonStartSettings>,
    operation: RequestOperation,
) -> Result<CompletedRequest, CliFailure> {
    let deadline = Instant::now()
        .checked_add(options.connect_timeout)
        .ok_or_else(|| CliFailure::internal("connect deadline overflow"))?;
    let context = ConnectionContext::open(options).map_err(ConnectError::into_failure)?;
    let acquisition_operation = start
        .as_ref()
        .map(|_| RequestOperation::Capabilities(CapabilitiesRequest::default()));
    loop {
        let request = acquisition_operation
            .clone()
            .unwrap_or_else(|| operation.clone());
        let acquisition_deadline = acquisition_operation.as_ref().map(|_| deadline);
        match request_once(options, &context, request, acquisition_deadline).await {
            Ok(probe) => {
                if acquisition_operation.is_none()
                    || matches!(operation, RequestOperation::Capabilities(_))
                {
                    return Ok(probe);
                }
                return request_once(options, &context, operation, None)
                    .await
                    .map_err(ConnectError::into_failure);
            }
            Err(error) if error.is_verified_generation_change() => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(error.into_failure());
                }
                tokio::time::sleep(CONNECT_RETRY_DELAY.min(remaining)).await;
            }
            Err(error) if start.is_some() && error.is_startup_pending() => break,
            Err(error) => return Err(error.into_failure()),
        }
    }

    let settings = start.expect("start settings checked");
    let mut child = Some(SpawnedDaemon::new(spawn_daemon(options, settings)?));
    let mut last_exit = None;
    let mut next_spawn = deadline;
    let mut spawn_retry_delay = CONNECT_RETRY_DELAY;
    loop {
        let now = Instant::now();
        if now >= deadline {
            let exit = last_exit
                .map(|status| format!("; spawned daemon exited with {status}"))
                .unwrap_or_default();
            return Err(CliFailure::unavailable(
                format!(
                    "daemon did not publish a verified loopback endpoint before the connect deadline{exit}"
                ),
                true,
            ));
        }

        if let Some(spawned) = child.as_mut()
            && let Some(status) = spawned.observe()?
        {
            last_exit = Some(status);
            child = None;
            next_spawn = now
                .checked_add(spawn_retry_delay)
                .unwrap_or(deadline)
                .min(deadline);
            spawn_retry_delay = spawn_retry_delay
                .checked_mul(2)
                .unwrap_or(MAX_START_RETRY_DELAY)
                .min(MAX_START_RETRY_DELAY);
        }

        match request_once(
            options,
            &context,
            RequestOperation::Capabilities(CapabilitiesRequest::default()),
            Some(deadline),
        )
        .await
        {
            Ok(probe) => {
                if let Some(mut spawned) = child.take() {
                    let _ = spawned.observe()?;
                    if spawned.process_id() == Some(probe.server_pid) {
                        spawned.detach_running();
                    }
                }
                if matches!(operation, RequestOperation::Capabilities(_)) {
                    return Ok(probe);
                }
                let completed = request_once(options, &context, operation, None)
                    .await
                    .map_err(ConnectError::into_failure)?;
                return Ok(completed);
            }
            Err(error) if error.is_verified_generation_change() || error.is_startup_pending() => {}
            Err(error) => return Err(error.into_failure()),
        }

        let now = Instant::now();
        if child.is_none() && now >= next_spawn {
            child = Some(SpawnedDaemon::new(spawn_daemon(options, settings)?));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            let until_spawn = if child.is_none() {
                next_spawn.saturating_duration_since(Instant::now())
            } else {
                CONNECT_RETRY_DELAY
            };
            tokio::time::sleep(CONNECT_RETRY_DELAY.min(until_spawn).min(remaining)).await;
        }
    }
}

async fn request_once(
    options: &ConnectionOptions,
    context: &ConnectionContext,
    operation: RequestOperation,
    acquisition_deadline: Option<Instant>,
) -> Result<CompletedRequest, ConnectError> {
    let discovered = context.discover_endpoint()?;
    let descriptor = discovered.descriptor();
    let binding = EndpointBinding {
        project_id: descriptor.project_id(),
        daemon_instance_id: descriptor.daemon_instance_id(),
        query_policy_id: descriptor.query_policy_id(),
    };
    let request = RequestEnvelope::new(
        descriptor.business_protocol_revision(),
        random_request_id().map_err(ConnectError::Failure)?,
        binding.project_id,
        binding.daemon_instance_id,
        binding.query_policy_id,
        operation,
    )
    .map_err(ConnectError::Contract)?;
    let encoded = encode_request_json(&request).map_err(ConnectError::Protocol)?;
    let host = descriptor.socket_addr().to_string();
    let url = format!("http://{host}{LOOPBACK_HTTP_REQUEST_PATH}");
    let host_header =
        HeaderValue::try_from(host.as_str()).map_err(|error| ConnectError::InvalidHeader {
            field: "Host",
            error,
        })?;
    let authorization = authorization_header(descriptor.capability())?;
    let response_timeout = request_timeout(
        request.operation(),
        options.request_timeout,
        acquisition_deadline,
    )?;
    let response = options
        .http
        .post(url)
        .header(HOST, host_header)
        .header(AUTHORIZATION, authorization)
        .header(CONTENT_TYPE, JSON_CONTENT_TYPE)
        .header(ACCEPT, JSON_CONTENT_TYPE)
        .header(CONNECTION, "close")
        .timeout(response_timeout)
        .body(encoded)
        .send()
        .await;
    let mut response = match response {
        Ok(response) => response,
        Err(source) => {
            context
                .project
                .revalidate()
                .map_err(ConnectError::Project)?;
            match discovered.ensure_unchanged(&context.namespace) {
                Err(EndpointStoreError::EndpointChanged) if source.is_connect() => {
                    return Err(ConnectError::GenerationChanged);
                }
                Err(error) => return Err(ConnectError::StoreAfterRequest(error)),
                Ok(()) if source.is_connect() => {
                    return Err(ConnectError::EndpointUnavailable(source));
                }
                Ok(()) => return Err(ConnectError::RequestFailed(source)),
            }
        }
    };

    let status = response.status();
    let content_type_is_json = response_content_type_is_json(response.headers());
    let maximum_response_bytes = max_response_json_bytes(request.operation().kind());
    let body = if status == StatusCode::OK {
        read_bounded_response(&mut response, maximum_response_bytes).await
    } else {
        read_bounded_error_response(&mut response, maximum_response_bytes).await
    };
    context
        .project
        .revalidate()
        .map_err(ConnectError::Project)?;
    let encoded = match body {
        Ok(encoded) => encoded,
        Err(error) => {
            discovered
                .ensure_unchanged(&context.namespace)
                .map_err(ConnectError::StoreAfterResponse)?;
            return Err(ConnectError::ResponseBody(error));
        }
    };
    if status != StatusCode::OK {
        discovered
            .ensure_unchanged(&context.namespace)
            .map_err(ConnectError::StoreAfterResponse)?;
        return Err(ConnectError::HttpStatus {
            status,
            detail: http_error_detail(&encoded),
        });
    }
    if !content_type_is_json {
        discovered
            .ensure_unchanged(&context.namespace)
            .map_err(ConnectError::StoreAfterResponse)?;
        return Err(ConnectError::InvalidContentType);
    }
    let mut budget = AssetLoadBudget::default();
    let response = match decode_response_json(&encoded, &mut budget, &request) {
        Ok(response) => response,
        Err(error) => {
            discovered
                .ensure_unchanged(&context.namespace)
                .map_err(ConnectError::StoreAfterResponse)?;
            return Err(ConnectError::Protocol(error));
        }
    };
    let outcome = response.into_outcome();
    let allow_missing_endpoint = successful_shutdown(&outcome);
    if allow_missing_endpoint {
        revalidate_after_shutdown(&discovered, &context.namespace)
            .map_err(ConnectError::StoreAfterResponse)?;
    } else {
        discovered
            .ensure_unchanged(&context.namespace)
            .map_err(ConnectError::StoreAfterResponse)?;
    }
    Ok(CompletedRequest {
        binding,
        outcome,
        server_pid: descriptor.server_pid(),
    })
}

fn authorization_header(
    capability: &unity_asset_search_local::HttpCapability,
) -> Result<HeaderValue, ConnectError> {
    let encoded = capability.encode_hex();
    let mut value = [0_u8; BEARER_PREFIX.len() + HTTP_CAPABILITY_HEX_BYTES];
    value[..BEARER_PREFIX.len()].copy_from_slice(BEARER_PREFIX);
    value[BEARER_PREFIX.len()..].copy_from_slice(&encoded);
    let mut header =
        HeaderValue::from_bytes(&value).map_err(|error| ConnectError::InvalidHeader {
            field: "Authorization",
            error,
        })?;
    value.fill(0);
    header.set_sensitive(true);
    Ok(header)
}

fn response_content_type_is_json(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return false;
    };
    values.next().is_none() && value.as_bytes() == JSON_CONTENT_TYPE.as_bytes()
}

fn successful_shutdown(outcome: &ResponseOutcome) -> bool {
    matches!(
        outcome,
        ResponseOutcome::Success(response) if matches!(
            response.as_ref(),
            ResponseOperation::Shutdown(response) if response.accepted
        )
    )
}

fn revalidate_after_shutdown(
    expected: &DiscoveredLoopbackEndpoint,
    namespace: &EndpointNamespaceV1,
) -> Result<(), EndpointStoreError> {
    match namespace.discover_loopback_endpoint() {
        Ok(current) if &current == expected => Ok(()),
        Ok(_) => Err(EndpointStoreError::EndpointChanged),
        Err(EndpointStoreError::DescriptorMissing) => Ok(()),
        Err(error) => Err(error),
    }
}

async fn read_bounded_response(
    response: &mut reqwest::Response,
    maximum: usize,
) -> Result<Vec<u8>, ResponseBodyError> {
    if let Some(content_length) = response.content_length()
        && content_length > maximum as u64
    {
        return Err(ResponseBodyError::TooLarge {
            requested: content_length,
            maximum,
        });
    }
    let initial_capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0)
        .min(maximum);
    let mut encoded = Vec::new();
    encoded
        .try_reserve(initial_capacity)
        .map_err(|_| ResponseBodyError::AllocationFailed {
            requested: initial_capacity,
        })?;
    while let Some(chunk) = response.chunk().await.map_err(ResponseBodyError::Read)? {
        let requested = encoded
            .len()
            .checked_add(chunk.len())
            .ok_or(ResponseBodyError::LengthOverflow)?;
        if requested > maximum {
            return Err(ResponseBodyError::TooLarge {
                requested: requested as u64,
                maximum,
            });
        }
        encoded
            .try_reserve(chunk.len())
            .map_err(|_| ResponseBodyError::AllocationFailed { requested })?;
        encoded.extend_from_slice(&chunk);
    }
    Ok(encoded)
}

async fn read_bounded_error_response(
    response: &mut reqwest::Response,
    maximum: usize,
) -> Result<Vec<u8>, ResponseBodyError> {
    const MAXIMUM_RETAINED: usize = 512;
    if let Some(content_length) = response.content_length()
        && content_length > maximum as u64
    {
        return Err(ResponseBodyError::TooLarge {
            requested: content_length,
            maximum,
        });
    }
    let mut total = 0_usize;
    let mut prefix = Vec::new();
    prefix
        .try_reserve(MAXIMUM_RETAINED)
        .map_err(|_| ResponseBodyError::AllocationFailed {
            requested: MAXIMUM_RETAINED,
        })?;
    while let Some(chunk) = response.chunk().await.map_err(ResponseBodyError::Read)? {
        total = total
            .checked_add(chunk.len())
            .ok_or(ResponseBodyError::LengthOverflow)?;
        if total > maximum {
            return Err(ResponseBodyError::TooLarge {
                requested: total as u64,
                maximum,
            });
        }
        let remaining = MAXIMUM_RETAINED.saturating_sub(prefix.len());
        prefix.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(prefix)
}

fn http_error_detail(encoded: &[u8]) -> String {
    if encoded.is_empty() {
        "empty response body".to_owned()
    } else {
        String::from_utf8_lossy(encoded).into_owned()
    }
}

fn operation_response_timeout(operation: &RequestOperation, configured: Duration) -> Duration {
    let RequestOperation::ReindexWait(request) = operation else {
        return configured;
    };
    Duration::from_millis(u64::from(request.timeout_ms))
        .saturating_add(SERVER_WAIT_RESPONSE_MARGIN)
        .max(configured)
}

fn request_timeout(
    operation: &RequestOperation,
    configured: Duration,
    acquisition_deadline: Option<Instant>,
) -> Result<Duration, ConnectError> {
    let operation_timeout = operation_response_timeout(operation, configured);
    let Some(deadline) = acquisition_deadline else {
        return Ok(operation_timeout);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(ConnectError::Failure(CliFailure::unavailable(
            "daemon acquisition deadline elapsed before the capabilities probe",
            true,
        )));
    }
    Ok(operation_timeout.min(remaining))
}

fn spawn_daemon(
    options: &ConnectionOptions,
    settings: &DaemonStartSettings,
) -> Result<Child, CliFailure> {
    let executable = resolve_daemon_binary(options)?;
    let mut command = Command::new(&executable);
    command
        .arg("--project-root")
        .arg(&options.project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(index_dir) = &options.index_dir {
        command.arg("--index-dir").arg(index_dir);
    }
    if settings.watch {
        command.arg("--watch");
    }
    if !settings.startup_reindex {
        command.arg("--no-startup-reindex");
    }
    if settings.scan_all {
        command.arg("--scan-all");
    }
    command.spawn().map_err(|error| {
        CliFailure::unavailable(
            format!("start daemon {}: {error}", executable.display()),
            false,
        )
    })
}

struct SpawnedDaemon {
    child: Option<Child>,
}

impl SpawnedDaemon {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn observe(&mut self) -> Result<Option<ExitStatus>, CliFailure> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let status = child
            .try_wait()
            .map_err(|error| CliFailure::internal(format!("observe daemon process: {error}")))?;
        if let Some(status) = status {
            self.child = None;
            return Ok(Some(status));
        }
        Ok(None)
    }

    fn process_id(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    fn detach_running(mut self) {
        self.child = None;
    }
}

impl Drop for SpawnedDaemon {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn resolve_daemon_binary(options: &ConnectionOptions) -> Result<PathBuf, CliFailure> {
    if let Some(path) = &options.daemon_binary {
        return Ok(path.clone());
    }
    if let Some(path) = std::env::var_os("UNITY_ASSET_SEARCH_DAEMON") {
        return Ok(path.into());
    }
    let current = std::env::current_exe()
        .map_err(|error| CliFailure::internal(format!("resolve CLI executable: {error}")))?;
    let sibling = current.with_file_name(if cfg!(windows) {
        "unity-asset-search-daemon.exe"
    } else {
        "unity-asset-search-daemon"
    });
    Ok(sibling)
}

fn random_request_id() -> Result<RequestId, CliFailure> {
    let mut random = rand::rngs::OsRng;
    for _ in 0..16 {
        let mut bytes = [0_u8; 16];
        random
            .try_fill_bytes(&mut bytes)
            .map_err(|error| CliFailure::internal(format!("obtain request entropy: {error}")))?;
        if bytes.iter().any(|byte| *byte != 0) {
            return Ok(RequestId::from_bytes(bytes));
        }
    }
    Err(CliFailure::internal(
        "operating-system entropy repeatedly returned a zero request ID",
    ))
}

fn map_store_failure(error: EndpointStoreError) -> CliFailure {
    let message = format!("endpoint discovery failed: {error}");
    match error {
        EndpointStoreError::LoopbackDescriptor(
            LoopbackEndpointDescriptorError::BindingMismatch { .. },
        ) => CliFailure::transport(message, false),
        EndpointStoreError::DescriptorMissing | EndpointStoreError::EndpointChanged => {
            CliFailure::unavailable(message, true)
        }
        _ => CliFailure::unavailable(message, false),
    }
}

#[derive(Debug, thiserror::Error)]
enum ResponseBodyError {
    #[error("response body is {requested} bytes; maximum is {maximum}")]
    TooLarge { requested: u64, maximum: usize },
    #[error("response body length overflowed usize")]
    LengthOverflow,
    #[error("could not reserve {requested} bytes for the response body")]
    AllocationFailed { requested: usize },
    #[error("could not read the response body: {0}")]
    Read(#[source] reqwest::Error),
}

#[derive(Debug, thiserror::Error)]
enum ConnectError {
    #[error("invalid Unity project root: {0}")]
    Project(#[source] unity_asset_search_local::ProjectLocatorError),
    #[error("private local root validation failed: {0}")]
    Roots(#[source] unity_asset_search_local::PrivateRootsError),
    #[error("endpoint discovery failed: {0}")]
    Store(#[source] EndpointStoreError),
    #[error("endpoint generation changed before a request connection was established")]
    GenerationChanged,
    #[error("endpoint validation failed after a request attempt: {0}")]
    StoreAfterRequest(#[source] EndpointStoreError),
    #[error("endpoint validation failed after receiving a response: {0}")]
    StoreAfterResponse(#[source] EndpointStoreError),
    #[error("loopback HTTP endpoint is unavailable: {0}")]
    EndpointUnavailable(#[source] reqwest::Error),
    #[error("loopback HTTP request failed after connection: {0}")]
    RequestFailed(#[source] reqwest::Error),
    #[error("loopback HTTP response body failed: {0}")]
    ResponseBody(#[source] ResponseBodyError),
    #[error("loopback HTTP response status {status}: {detail}")]
    HttpStatus { status: StatusCode, detail: String },
    #[error("loopback HTTP response must have exactly one application/json Content-Type")]
    InvalidContentType,
    #[error("invalid {field} HTTP header: {error}")]
    InvalidHeader {
        field: &'static str,
        #[source]
        error: reqwest::header::InvalidHeaderValue,
    },
    #[error("protocol JSON failed: {0}")]
    Protocol(#[source] unity_asset_search_protocol::ProtocolJsonError),
    #[error("protocol validation failed: {0}")]
    Contract(#[source] unity_asset_search_protocol::ContractValidationError),
    #[error("{0:?}")]
    Failure(CliFailure),
}

impl ConnectError {
    fn is_verified_generation_change(&self) -> bool {
        matches!(self, Self::GenerationChanged)
    }

    fn is_startup_pending(&self) -> bool {
        match self {
            Self::Store(EndpointStoreError::DescriptorMissing) | Self::EndpointUnavailable(_) => {
                true
            }
            Self::RequestFailed(error) => error.is_timeout(),
            _ => false,
        }
    }

    fn into_failure(self) -> CliFailure {
        match self {
            Self::Project(error) => {
                CliFailure::input(format!("invalid Unity project root: {error}"))
            }
            Self::Roots(error) => CliFailure::unavailable(
                format!("private local root validation failed: {error}"),
                false,
            ),
            Self::Store(error) => map_store_failure(error),
            Self::GenerationChanged => CliFailure::unavailable(
                "endpoint generation changed before the request connected",
                true,
            ),
            Self::StoreAfterRequest(error) => CliFailure::transport(
                format!(
                    "endpoint changed after a request attempt; the operation may have completed: {error}"
                ),
                false,
            ),
            Self::StoreAfterResponse(error) => CliFailure::transport(
                format!("endpoint changed after the response was received: {error}"),
                false,
            ),
            Self::EndpointUnavailable(error) => CliFailure::transport(
                format!("loopback HTTP endpoint is unavailable: {error}"),
                true,
            ),
            Self::RequestFailed(error) => CliFailure::transport(
                format!("loopback HTTP request failed; the operation may have completed: {error}"),
                false,
            ),
            Self::ResponseBody(error) => CliFailure::transport(
                format!("loopback HTTP response body failed: {error}"),
                false,
            ),
            Self::HttpStatus { status, detail } if status.is_server_error() => {
                CliFailure::unavailable(
                    format!("loopback HTTP server returned {status}: {detail}"),
                    false,
                )
            }
            Self::HttpStatus { status, detail } => CliFailure::protocol(format!(
                "loopback HTTP server rejected the request with {status}: {detail}"
            )),
            Self::InvalidContentType => CliFailure::protocol(
                "loopback HTTP response must have exactly one application/json Content-Type",
            ),
            Self::InvalidHeader { field, error } => {
                CliFailure::internal(format!("construct {field} HTTP header: {error}"))
            }
            Self::Protocol(error) => CliFailure::protocol(format!("protocol JSON failed: {error}")),
            Self::Contract(error) => {
                CliFailure::protocol(format!("protocol validation failed: {error}"))
            }
            Self::Failure(failure) => failure,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
    use unity_asset_search_protocol::{
        CapabilitiesRequest, OperationId, ReindexWaitRequest, RequestOperation, ResponseOperation,
        ResponseOutcome, ShutdownResponse,
    };

    use super::{
        SERVER_WAIT_RESPONSE_MARGIN, operation_response_timeout, request_timeout,
        response_content_type_is_json, successful_shutdown,
    };

    #[test]
    fn reindex_wait_timeout_includes_server_margin() {
        let request = RequestOperation::ReindexWait(ReindexWaitRequest {
            operation_id: OperationId::from_bytes([0x33; 16]),
            timeout_ms: 4_000,
        });
        assert_eq!(
            operation_response_timeout(&request, Duration::from_secs(1)),
            Duration::from_millis(4_000) + SERVER_WAIT_RESPONSE_MARGIN
        );
    }

    #[test]
    fn acquisition_probe_uses_only_the_remaining_connect_deadline() {
        let request = RequestOperation::Capabilities(CapabilitiesRequest::default());
        let maximum = Duration::from_millis(100);
        let timeout = request_timeout(
            &request,
            Duration::from_secs(60),
            Some(Instant::now() + maximum),
        )
        .unwrap();

        assert!(!timeout.is_zero());
        assert!(timeout <= maximum);
    }

    #[test]
    fn response_content_type_requires_one_exact_json_value() {
        let mut headers = HeaderMap::new();
        assert!(!response_content_type_is_json(&headers));

        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        assert!(response_content_type_is_json(&headers));

        headers.append(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        assert!(!response_content_type_is_json(&headers));

        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        assert!(!response_content_type_is_json(&headers));
    }

    #[test]
    fn only_an_accepted_shutdown_response_allows_endpoint_withdrawal() {
        let accepted =
            ResponseOutcome::Success(Box::new(ResponseOperation::Shutdown(ShutdownResponse {
                accepted: true,
            })));
        assert!(successful_shutdown(&accepted));

        let rejected =
            ResponseOutcome::Success(Box::new(ResponseOperation::Shutdown(ShutdownResponse {
                accepted: false,
            })));
        assert!(!successful_shutdown(&rejected));
    }
}
