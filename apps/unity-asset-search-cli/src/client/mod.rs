use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rand::TryRngCore as _;
use unity_asset_core::AssetLoadBudget;
use unity_asset_search_local::{
    EndpointDescriptorError, EndpointStoreError, EndpointTransportError, FrameReadTimeoutsV1,
    PrivateRootsV1, ProjectLocatorV1, VerifiedFramedTransportV1,
};
use unity_asset_search_protocol::{
    BUSINESS_PROTOCOL_REVISION, BootstrapErrorCode, BootstrapHelloV2, BootstrapReplyV2,
    DaemonInstanceId, FrameLimits, ProjectId, QueryPolicyId, RequestEnvelope, RequestId,
    RequestOperation, ResponseOperation, ResponseOutcome, decode_response_frame,
    decode_validated_frame, encode_frame, encode_request_frame,
};

use crate::command::{Args, DaemonStartSettings};
use crate::output::CliFailure;

const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(50);
const MAX_START_RETRY_DELAY: Duration = Duration::from_secs(1);
const SERVER_WAIT_RESPONSE_MARGIN: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct ConnectionOptions {
    project_root: PathBuf,
    index_dir: Option<PathBuf>,
    daemon_binary: Option<PathBuf>,
    connect_timeout: Duration,
    request_timeout: Duration,
}

impl ConnectionOptions {
    pub fn from_args(args: &Args) -> Result<Self, CliFailure> {
        Ok(Self {
            project_root: args.project_root()?,
            index_dir: args.index_dir(),
            daemon_binary: args.daemon_binary(),
            connect_timeout: args.connect_timeout(),
            request_timeout: args.request_timeout(),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SessionBinding {
    pub project_id: ProjectId,
    pub daemon_instance_id: DaemonInstanceId,
    pub query_policy_id: QueryPolicyId,
    pub protocol_revision: u16,
}

pub struct ClientSession {
    stream: VerifiedFramedTransportV1,
    binding: SessionBinding,
    server_pid: u32,
    request_timeout: Duration,
}

impl ClientSession {
    #[must_use]
    pub const fn binding(&self) -> SessionBinding {
        self.binding
    }

    pub async fn execute(
        &mut self,
        operation: RequestOperation,
    ) -> Result<ResponseOperation, CliFailure> {
        let response_timeout = operation_response_timeout(&operation, self.request_timeout);
        let request = RequestEnvelope::new(
            self.binding.protocol_revision,
            random_request_id()?,
            self.binding.project_id,
            self.binding.daemon_instance_id,
            self.binding.query_policy_id,
            operation,
        )
        .map_err(|error| CliFailure::input(format!("invalid request: {error}")))?;
        let frame = encode_request_frame(&request)
            .map_err(|error| CliFailure::protocol(format!("encode request frame: {error}")))?;
        write_frame(
            &mut self.stream,
            &frame,
            FrameLimits::request_envelope(),
            self.request_timeout,
        )
        .await?;
        let limits = FrameLimits::response(request.operation().kind());
        let response_frame = read_required_frame(
            &mut self.stream,
            limits,
            response_timeout,
            "business response",
        )
        .await?;
        let mut budget = AssetLoadBudget::default();
        let response = decode_response_frame(&response_frame, &mut budget, &request)
            .map_err(|error| CliFailure::protocol(format!("decode response frame: {error}")))?;
        match response.into_outcome() {
            ResponseOutcome::Success(operation) => Ok(*operation),
            ResponseOutcome::Error(error) => Err(CliFailure::daemon(*error)),
        }
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

pub async fn connect(
    options: &ConnectionOptions,
    start: Option<&DaemonStartSettings>,
) -> Result<(ClientSession, bool), CliFailure> {
    let deadline = Instant::now()
        .checked_add(options.connect_timeout)
        .ok_or_else(|| CliFailure::internal("connect deadline overflow"))?;
    loop {
        match connect_once(options, deadline).await {
            Ok(session) => return Ok((session, false)),
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
    let mut child: Option<SpawnedDaemon> = None;
    let mut last_exit = None;
    let mut next_spawn = Instant::now();
    let mut spawn_retry_delay = CONNECT_RETRY_DELAY;
    loop {
        let now = Instant::now();
        if now >= deadline {
            let exit = last_exit
                .map(|status| format!("; spawned daemon exited with {status}"))
                .unwrap_or_default();
            return Err(CliFailure::unavailable(
                format!(
                    "daemon did not publish a verified endpoint before the connect deadline{exit}"
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

        match connect_once(options, deadline).await {
            Ok(session) => {
                let started = if let Some(mut spawned) = child.take() {
                    let _ = spawned.observe()?;
                    if spawned.process_id() == Some(session.server_pid) {
                        spawned.detach_running()
                    } else {
                        false
                    }
                } else {
                    false
                };
                return Ok((session, started));
            }
            Err(error) if error.is_verified_generation_change() || error.is_startup_pending() => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if !remaining.is_zero() {
                    tokio::time::sleep(CONNECT_RETRY_DELAY.min(remaining)).await;
                }
            }
            Err(error) => return Err(error.into_failure()),
        }

        let now = Instant::now();
        if child.is_none() {
            if now < next_spawn {
                tokio::time::sleep(next_spawn.min(deadline).saturating_duration_since(now)).await;
                continue;
            }
            child = Some(SpawnedDaemon::new(spawn_daemon(options, settings)?));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            tokio::time::sleep(CONNECT_RETRY_DELAY.min(remaining)).await;
        }
    }
}

async fn connect_once(
    options: &ConnectionOptions,
    deadline: Instant,
) -> Result<ClientSession, ConnectError> {
    let project = ProjectLocatorV1::open(&options.project_root).map_err(ConnectError::Project)?;
    let roots = PrivateRootsV1::discover_for_current_context().map_err(ConnectError::Roots)?;
    let namespace = roots
        .runtime()
        .endpoint_namespace(project.project_id())
        .map_err(ConnectError::Roots)?;
    let discovered = namespace.discover_endpoint().map_err(ConnectError::Store)?;
    let descriptor = discovered.descriptor();
    let daemon_instance_id = descriptor.daemon_instance_id();
    let mut stream = discovered
        .connect_verified(&namespace, deadline)
        .await
        .map_err(ConnectError::Transport)?;

    let hello = BootstrapHelloV2::new(
        project.project_id(),
        daemon_instance_id,
        vec![BUSINESS_PROTOCOL_REVISION],
    )
    .map_err(ConnectError::Contract)?;
    let frame = encode_frame(&hello, FrameLimits::bootstrap()).map_err(ConnectError::Framing)?;
    write_frame(
        &mut stream,
        &frame,
        FrameLimits::bootstrap(),
        remaining_timeout(deadline, options.request_timeout)?,
    )
    .await
    .map_err(ConnectError::Failure)?;
    let reply_frame = read_required_frame(
        &mut stream,
        FrameLimits::bootstrap(),
        remaining_timeout(deadline, options.request_timeout)?,
        "bootstrap reply",
    )
    .await
    .map_err(ConnectError::Failure)?;
    let mut budget = AssetLoadBudget::default();
    let reply: BootstrapReplyV2 =
        decode_validated_frame(&reply_frame, &mut budget, FrameLimits::bootstrap())
            .map_err(ConnectError::Framing)?;
    reply.validate_for(&hello).map_err(ConnectError::Contract)?;
    let selected_revision = reply
        .selected_revision()
        .ok_or_else(|| ConnectError::Rejected(rejection_code(&reply)))?;
    let query_policy_id = reply
        .query_policy_id()
        .ok_or(ConnectError::MissingQueryPolicy)?;
    discovered
        .ensure_unchanged(&namespace)
        .map_err(ConnectError::Store)?;
    project.revalidate().map_err(ConnectError::Project)?;

    Ok(ClientSession {
        stream,
        binding: SessionBinding {
            project_id: project.project_id(),
            daemon_instance_id,
            query_policy_id,
            protocol_revision: selected_revision,
        },
        server_pid: descriptor.server_pid(),
        request_timeout: options.request_timeout,
    })
}

fn remaining_timeout(deadline: Instant, configured: Duration) -> Result<Duration, ConnectError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(ConnectError::Failure(CliFailure::transport(
            "local IPC connect/bootstrap deadline elapsed",
            true,
        )));
    }
    Ok(configured.min(remaining))
}

fn rejection_code(reply: &BootstrapReplyV2) -> BootstrapErrorCode {
    match reply {
        BootstrapReplyV2::Rejected { code, .. } => *code,
        BootstrapReplyV2::Accepted { .. } => BootstrapErrorCode::NoCommonRevision,
    }
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

    fn detach_running(mut self) -> bool {
        self.child.take().is_some()
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
    if sibling.is_file() {
        Ok(sibling)
    } else {
        Err(CliFailure::unavailable(
            format!(
                "daemon binary is not installed beside the CLI at {}; pass --daemon-binary or set UNITY_ASSET_SEARCH_DAEMON",
                sibling.display()
            ),
            false,
        ))
    }
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

async fn write_frame(
    stream: &mut VerifiedFramedTransportV1,
    frame: &[u8],
    limits: FrameLimits,
    timeout: Duration,
) -> Result<(), CliFailure> {
    stream
        .write_frame(frame, limits, timeout)
        .await
        .map_err(map_transport)
}

async fn read_required_frame(
    stream: &mut VerifiedFramedTransportV1,
    limits: FrameLimits,
    timeout: Duration,
    label: &'static str,
) -> Result<Vec<u8>, CliFailure> {
    tokio::time::timeout(
        timeout,
        stream.read_frame(limits, FrameReadTimeoutsV1::uniform(timeout)),
    )
    .await
    .map_err(|_| CliFailure::transport(format!("{label} deadline elapsed"), true))?
    .map_err(map_transport)?
    .ok_or_else(|| CliFailure::transport(format!("peer closed before {label}"), true))
}

fn map_transport(error: EndpointTransportError) -> CliFailure {
    let message = format!("local IPC transport: {error}");
    match error {
        EndpointTransportError::FrameTooLarge { .. }
        | EndpointTransportError::FrameLengthOverflow
        | EndpointTransportError::InvalidEncodedFrame { .. } => CliFailure::protocol(message),
        EndpointTransportError::FrameAllocationFailed { .. }
        | EndpointTransportError::FrameDeadlineOverflow => CliFailure::internal(message),
        EndpointTransportError::DeadlineElapsed
        | EndpointTransportError::FrameReadDeadlineElapsed
        | EndpointTransportError::FrameWriteDeadlineElapsed
        | EndpointTransportError::EndpointUnavailable
        | EndpointTransportError::Io { .. }
        | EndpointTransportError::Store(
            EndpointStoreError::DescriptorMissing | EndpointStoreError::EndpointChanged,
        ) => CliFailure::transport(message, true),
        EndpointTransportError::PeerContextMismatch
        | EndpointTransportError::PeerIdentityMismatch => CliFailure::transport(message, false),
        _ => CliFailure::transport(message, false),
    }
}

fn map_store_failure(error: EndpointStoreError) -> CliFailure {
    let message = format!("endpoint discovery failed: {error}");
    match error {
        EndpointStoreError::Descriptor(EndpointDescriptorError::BindingMismatch { .. }) => {
            CliFailure::transport(message, false)
        }
        EndpointStoreError::DescriptorMissing | EndpointStoreError::EndpointChanged => {
            CliFailure::unavailable(message, true)
        }
        _ => CliFailure::unavailable(message, false),
    }
}

#[derive(Debug, thiserror::Error)]
enum ConnectError {
    #[error("invalid Unity project root: {0}")]
    Project(#[source] unity_asset_search_local::ProjectLocatorError),
    #[error("private local root validation failed: {0}")]
    Roots(#[source] unity_asset_search_local::PrivateRootsError),
    #[error("endpoint discovery failed: {0}")]
    Store(#[source] EndpointStoreError),
    #[error("local IPC connection failed: {0}")]
    Transport(#[source] EndpointTransportError),
    #[error("protocol framing failed: {0}")]
    Framing(#[source] unity_asset_search_protocol::FramingError),
    #[error("protocol validation failed: {0}")]
    Contract(#[source] unity_asset_search_protocol::ContractValidationError),
    #[error("bootstrap was rejected: {0:?}")]
    Rejected(BootstrapErrorCode),
    #[error("accepted bootstrap reply omitted the query policy identity")]
    MissingQueryPolicy,
    #[error("{0:?}")]
    Failure(CliFailure),
}

impl ConnectError {
    fn is_verified_generation_change(&self) -> bool {
        matches!(
            self,
            Self::Store(EndpointStoreError::EndpointChanged)
                | Self::Transport(EndpointTransportError::Store(
                    EndpointStoreError::EndpointChanged,
                ))
        )
    }

    fn is_startup_pending(&self) -> bool {
        matches!(
            self,
            Self::Store(EndpointStoreError::DescriptorMissing)
                | Self::Transport(EndpointTransportError::EndpointUnavailable)
                | Self::Transport(EndpointTransportError::Store(
                    EndpointStoreError::DescriptorMissing,
                ))
        )
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
            Self::Transport(error) => map_transport(error),
            Self::Framing(error) => {
                CliFailure::protocol(format!("protocol framing failed: {error}"))
            }
            Self::Contract(error) => {
                CliFailure::protocol(format!("protocol validation failed: {error}"))
            }
            Self::Rejected(code) => {
                CliFailure::protocol(format!("bootstrap was rejected: {code:?}"))
            }
            Self::MissingQueryPolicy => {
                CliFailure::protocol("accepted bootstrap reply omitted the query policy identity")
            }
            Self::Failure(failure) => failure,
        }
    }
}

impl From<io::Error> for ConnectError {
    fn from(error: io::Error) -> Self {
        Self::Failure(CliFailure::transport(error.to_string(), true))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, Instant};

    use unity_asset_core::AssetLoadBudget;
    use unity_asset_search_local::{
        EndpointCleanupV1, EndpointDescriptorError, EndpointStoreError, EndpointTransportError,
        FrameReadTimeoutsV1, PrivateRootsV1, ProjectLocatorError, ProjectLocatorV1,
        generate_daemon_instance_id,
    };
    use unity_asset_search_protocol::{
        BUSINESS_PROTOCOL_REVISION, BootstrapHelloV2, BootstrapReplyV2, FrameLimits, QueryPolicyId,
        decode_validated_frame, encode_frame,
    };

    use super::{ConnectError, ConnectionOptions, connect_once};

    #[tokio::test]
    async fn bootstrap_cannot_commit_a_session_after_endpoint_withdrawal() {
        let project_root = tempfile::tempdir().expect("temporary Unity project");
        fs::create_dir(project_root.path().join("Assets")).expect("Assets marker");
        fs::create_dir(project_root.path().join("ProjectSettings"))
            .expect("ProjectSettings marker");
        let project = ProjectLocatorV1::open(project_root.path()).expect("locate project");
        let roots = PrivateRootsV1::discover_for_current_context().expect("private roots");
        let namespace = roots
            .runtime()
            .endpoint_namespace(project.project_id())
            .expect("endpoint namespace");
        let cleanup_path = namespace.path().to_path_buf();
        let mut claim = namespace
            .claim_daemon_endpoint()
            .expect("claim daemon endpoint");
        let instance = generate_daemon_instance_id().expect("daemon instance");
        let mut endpoint = claim.publish(instance).expect("publish endpoint");
        let options = ConnectionOptions {
            project_root: project_root.path().to_path_buf(),
            index_dir: None,
            daemon_binary: None,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(10),
        };

        let client = tokio::spawn(async move {
            connect_once(&options, Instant::now() + options.connect_timeout).await
        });
        let mut server = endpoint.accept_verified().await.expect("accept client");
        let hello_frame = server
            .read_frame(
                FrameLimits::bootstrap(),
                FrameReadTimeoutsV1::uniform(Duration::from_secs(10)),
            )
            .await
            .expect("read bootstrap hello")
            .expect("client sent bootstrap hello");
        let mut budget = AssetLoadBudget::default();
        let hello: BootstrapHelloV2 =
            decode_validated_frame(&hello_frame, &mut budget, FrameLimits::bootstrap())
                .expect("decode bootstrap hello");
        let reply = BootstrapReplyV2::negotiate(
            &hello,
            project.project_id(),
            instance,
            QueryPolicyId::from_bytes([0x44; 32]),
            &[BUSINESS_PROTOCOL_REVISION],
        );
        let reply_frame =
            encode_frame(&reply, FrameLimits::bootstrap()).expect("encode bootstrap reply");
        assert_eq!(
            endpoint.withdraw().expect("withdraw endpoint publication"),
            EndpointCleanupV1::Removed
        );
        server
            .write_frame(
                &reply_frame,
                FrameLimits::bootstrap(),
                Duration::from_secs(10),
            )
            .await
            .expect("write bootstrap reply");

        assert!(matches!(
            client.await.expect("join client"),
            Err(ConnectError::Store(EndpointStoreError::EndpointChanged))
        ));
        let post_bootstrap = server
            .read_frame(
                FrameLimits::request_envelope(),
                FrameReadTimeoutsV1::uniform(Duration::from_secs(1)),
            )
            .await
            .expect("observe client disconnect");
        assert!(post_bootstrap.is_none());

        drop(server);
        drop(endpoint);
        drop(claim);
        drop(namespace);
        drop(roots);
        for name in ["binding.v1", ".binding-v1.lock", ".daemon-v1.lock"] {
            let result = fs::remove_file(cleanup_path.join(name));
            assert!(
                result.is_ok()
                    || result.is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            );
        }
        fs::remove_dir(cleanup_path).expect("remove endpoint namespace");
    }

    #[tokio::test]
    async fn bootstrap_cannot_commit_a_session_after_project_replacement() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let project_root = temporary.path().join("project");
        let displaced_root = temporary.path().join("displaced-project");
        fs::create_dir_all(project_root.join("Assets")).expect("Assets marker");
        fs::create_dir_all(project_root.join("ProjectSettings")).expect("ProjectSettings marker");
        let project = ProjectLocatorV1::open(&project_root).expect("locate project");
        let roots = PrivateRootsV1::discover_for_current_context().expect("private roots");
        let namespace = roots
            .runtime()
            .endpoint_namespace(project.project_id())
            .expect("endpoint namespace");
        let cleanup_path = namespace.path().to_path_buf();
        let mut claim = namespace
            .claim_daemon_endpoint()
            .expect("claim daemon endpoint");
        let instance = generate_daemon_instance_id().expect("daemon instance");
        let mut endpoint = claim.publish(instance).expect("publish endpoint");
        let options = ConnectionOptions {
            project_root: project_root.clone(),
            index_dir: None,
            daemon_binary: None,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(10),
        };

        let client = tokio::spawn(async move {
            connect_once(&options, Instant::now() + options.connect_timeout).await
        });
        let mut server = endpoint.accept_verified().await.expect("accept client");
        let hello_frame = server
            .read_frame(
                FrameLimits::bootstrap(),
                FrameReadTimeoutsV1::uniform(Duration::from_secs(10)),
            )
            .await
            .expect("read bootstrap hello")
            .expect("client sent bootstrap hello");
        let mut budget = AssetLoadBudget::default();
        let hello: BootstrapHelloV2 =
            decode_validated_frame(&hello_frame, &mut budget, FrameLimits::bootstrap())
                .expect("decode bootstrap hello");
        let reply = BootstrapReplyV2::negotiate(
            &hello,
            project.project_id(),
            instance,
            QueryPolicyId::from_bytes([0x45; 32]),
            &[BUSINESS_PROTOCOL_REVISION],
        );
        let reply_frame =
            encode_frame(&reply, FrameLimits::bootstrap()).expect("encode bootstrap reply");
        fs::rename(&project_root, &displaced_root).expect("displace project root");
        fs::create_dir_all(project_root.join("Assets")).expect("replacement Assets marker");
        fs::create_dir_all(project_root.join("ProjectSettings"))
            .expect("replacement ProjectSettings marker");
        server
            .write_frame(
                &reply_frame,
                FrameLimits::bootstrap(),
                Duration::from_secs(10),
            )
            .await
            .expect("write bootstrap reply");

        assert!(matches!(
            client.await.expect("join client"),
            Err(ConnectError::Project(
                ProjectLocatorError::IdentityChanged { .. }
            ))
        ));
        let post_bootstrap = server
            .read_frame(
                FrameLimits::request_envelope(),
                FrameReadTimeoutsV1::uniform(Duration::from_secs(1)),
            )
            .await
            .expect("observe client disconnect");
        assert!(post_bootstrap.is_none());

        drop(server);
        drop(endpoint);
        drop(claim);
        drop(namespace);
        drop(roots);
        for name in ["binding.v1", ".binding-v1.lock", ".daemon-v1.lock"] {
            let result = fs::remove_file(cleanup_path.join(name));
            assert!(
                result.is_ok()
                    || result.is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            );
        }
        fs::remove_dir(cleanup_path).expect("remove endpoint namespace");
    }

    #[test]
    fn only_startup_pending_endpoints_authorize_daemon_start() {
        assert!(ConnectError::Store(EndpointStoreError::DescriptorMissing).is_startup_pending());
        assert!(
            ConnectError::Transport(EndpointTransportError::EndpointUnavailable)
                .is_startup_pending()
        );
        assert!(
            !ConnectError::Transport(EndpointTransportError::PeerContextMismatch)
                .is_startup_pending()
        );
        assert!(!ConnectError::Store(EndpointStoreError::EndpointChanged).is_startup_pending());
    }

    #[test]
    fn only_verified_generation_changes_are_retryable_after_spawn() {
        assert!(
            ConnectError::Store(EndpointStoreError::EndpointChanged)
                .is_verified_generation_change()
        );
        assert!(
            ConnectError::Transport(EndpointTransportError::Store(
                EndpointStoreError::EndpointChanged
            ))
            .is_verified_generation_change()
        );
        assert!(
            !ConnectError::Transport(EndpointTransportError::Store(
                EndpointStoreError::DescriptorMissing,
            ))
            .is_verified_generation_change()
        );
        assert!(
            !ConnectError::Transport(EndpointTransportError::EndpointUnavailable)
                .is_verified_generation_change()
        );
        for field in [
            "server_pid",
            "process_start_identity",
            "security_context_id",
        ] {
            assert!(
                !ConnectError::Transport(EndpointTransportError::Descriptor(
                    EndpointDescriptorError::BindingMismatch { field }
                ))
                .is_verified_generation_change()
            );
        }
        assert!(
            !ConnectError::Transport(EndpointTransportError::PeerContextMismatch)
                .is_verified_generation_change()
        );
        assert!(
            !ConnectError::Transport(EndpointTransportError::PeerIdentityMismatch)
                .is_verified_generation_change()
        );
    }

    #[test]
    fn startup_pending_states_are_not_generation_changes() {
        for error in [
            ConnectError::Store(EndpointStoreError::DescriptorMissing),
            ConnectError::Transport(EndpointTransportError::EndpointUnavailable),
            ConnectError::Transport(EndpointTransportError::Store(
                EndpointStoreError::DescriptorMissing,
            )),
        ] {
            assert!(error.is_startup_pending());
            assert!(!error.is_verified_generation_change());
        }
        assert!(!ConnectError::Store(EndpointStoreError::EndpointChanged).is_startup_pending());
    }

    #[test]
    fn descriptor_binding_mismatch_is_not_retryable_in_cli_output() {
        let failure = ConnectError::Store(EndpointStoreError::Descriptor(
            EndpointDescriptorError::BindingMismatch {
                field: "project_id",
            },
        ))
        .into_failure();
        assert!(!failure.is_retryable());
    }
}
