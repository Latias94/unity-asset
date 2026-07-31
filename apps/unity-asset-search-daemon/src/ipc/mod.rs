mod dispatch;

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use unity_asset_core::AssetLoadBudget;
use unity_asset_search_local::{
    ClaimedEndpointV1, EndpointCleanupV1, EndpointTransportError, MAX_LOCAL_IPC_CONNECTIONS_V1,
    VerifiedLocalStreamV1,
};
use unity_asset_search_protocol::{
    ApiError, ApiErrorCode, BUSINESS_PROTOCOL_REVISION, BootstrapHelloV2, BootstrapReplyV2,
    FrameLimits, OperationKind, ResponseEnvelope, decode_request_frame, decode_validated_frame,
    encode_frame, encode_response_frame,
};

pub(crate) use dispatch::DispatcherShutdown;
pub use dispatch::{Dispatcher, OperationRegistry, OperationRegistryOwner};

const MAX_WORK_IN_FLIGHT: usize = 16;
const MAX_WAIT_IN_FLIGHT: usize = 16;
const MAX_CONTROL_IN_FLIGHT: usize = 16;
const CONTROL_RESERVED_CONNECTIONS: usize = 8;
const ORDINARY_CONNECTIONS: usize = MAX_LOCAL_IPC_CONNECTIONS_V1 - CONTROL_RESERVED_CONNECTIONS;
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROL_RESERVED_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_RESERVED_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const BODY_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
const FATAL_DRAIN_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct DispatchCapacity {
    work: Arc<Semaphore>,
    wait: Arc<Semaphore>,
    control: Arc<Semaphore>,
}

impl DispatchCapacity {
    fn production() -> Self {
        Self::new(
            MAX_WORK_IN_FLIGHT,
            MAX_WAIT_IN_FLIGHT,
            MAX_CONTROL_IN_FLIGHT,
        )
    }

    fn new(work: usize, wait: usize, control: usize) -> Self {
        Self {
            work: Arc::new(Semaphore::new(work)),
            wait: Arc::new(Semaphore::new(wait)),
            control: Arc::new(Semaphore::new(control)),
        }
    }

    fn try_acquire(
        &self,
        class: DispatchClass,
    ) -> Result<OwnedSemaphorePermit, tokio::sync::TryAcquireError> {
        match class {
            DispatchClass::Work => Arc::clone(&self.work).try_acquire_owned(),
            DispatchClass::Wait => Arc::clone(&self.wait).try_acquire_owned(),
            DispatchClass::Control => Arc::clone(&self.control).try_acquire_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchClass {
    Work,
    Wait,
    Control,
}

#[derive(Clone)]
struct ConnectionCapacity {
    ordinary: Arc<Semaphore>,
    control_reserved: Arc<Semaphore>,
}

impl ConnectionCapacity {
    fn production() -> Self {
        Self::new(ORDINARY_CONNECTIONS, CONTROL_RESERVED_CONNECTIONS)
    }

    fn new(ordinary: usize, control_reserved: usize) -> Self {
        Self {
            ordinary: Arc::new(Semaphore::new(ordinary)),
            control_reserved: Arc::new(Semaphore::new(control_reserved)),
        }
    }

    async fn acquire(&self) -> SessionLease {
        if let Ok(permit) = Arc::clone(&self.ordinary).try_acquire_owned() {
            return SessionLease::new(SessionLane::Ordinary, permit);
        }
        let ordinary = Arc::clone(&self.ordinary).acquire_owned();
        let control_reserved = Arc::clone(&self.control_reserved).acquire_owned();
        tokio::select! {
            biased;
            permit = ordinary => SessionLease::new(
                SessionLane::Ordinary,
                permit.expect("ordinary connection semaphore remains open"),
            ),
            permit = control_reserved => SessionLease::new(
                SessionLane::ControlReserved,
                permit.expect("control-reserved connection semaphore remains open"),
            ),
        }
    }
}

struct SessionLease {
    lane: SessionLane,
    _permit: OwnedSemaphorePermit,
}

impl SessionLease {
    fn new(lane: SessionLane, permit: OwnedSemaphorePermit) -> Self {
        Self {
            lane,
            _permit: permit,
        }
    }

    const fn lane(&self) -> SessionLane {
        self.lane
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionLane {
    Ordinary,
    ControlReserved,
}

impl SessionLane {
    const fn bootstrap_timeout(self) -> Duration {
        match self {
            Self::Ordinary => BOOTSTRAP_TIMEOUT,
            Self::ControlReserved => CONTROL_RESERVED_BOOTSTRAP_TIMEOUT,
        }
    }

    const fn request_timeout(self) -> Duration {
        match self {
            Self::Ordinary => IDLE_TIMEOUT,
            Self::ControlReserved => CONTROL_RESERVED_REQUEST_TIMEOUT,
        }
    }

    const fn permits(self, class: DispatchClass) -> bool {
        matches!(self, Self::Ordinary) || matches!(class, DispatchClass::Control)
    }

    const fn single_request(self) -> bool {
        matches!(self, Self::ControlReserved)
    }
}

impl DispatchClass {
    const fn for_operation(kind: OperationKind) -> Self {
        match kind {
            OperationKind::Search
            | OperationKind::Suggest
            | OperationKind::References
            | OperationKind::ReindexAdmit => Self::Work,
            OperationKind::ReindexWait => Self::Wait,
            OperationKind::Capabilities
            | OperationKind::Status
            | OperationKind::ReindexStatus
            | OperationKind::ReindexCancel
            | OperationKind::Shutdown => Self::Control,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Wait => "wait",
            Self::Control => "control",
        }
    }

    const fn maximum(self) -> usize {
        match self {
            Self::Work => MAX_WORK_IN_FLIGHT,
            Self::Wait => MAX_WAIT_IN_FLIGHT,
            Self::Control => MAX_CONTROL_IN_FLIGHT,
        }
    }
}

/// Process-lifetime owner for accepted local IPC sessions.
///
/// This object deliberately outlives the serving future. If that future panics, the supervisor
/// still owns its `JoinSet` and can explicitly abort and join each session before releasing the
/// endpoint or index-writer leases.
pub(crate) struct IpcService {
    dispatcher: Dispatcher,
    connections: ConnectionCapacity,
    dispatch_capacity: DispatchCapacity,
    shutdown: watch::Receiver<Option<Instant>>,
    sessions: JoinSet<()>,
    rejection_log: PeerRejectionLog,
    #[cfg(test)]
    panic_after_session_spawn: bool,
}

impl IpcService {
    pub(crate) fn new(dispatcher: Dispatcher) -> Self {
        Self {
            shutdown: dispatcher.subscribe_shutdown(),
            dispatcher,
            connections: ConnectionCapacity::production(),
            dispatch_capacity: DispatchCapacity::production(),
            sessions: JoinSet::new(),
            rejection_log: PeerRejectionLog::new(),
            #[cfg(test)]
            panic_after_session_spawn: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_panic_after_session_spawn(mut self, enabled: bool) -> Self {
        self.panic_after_session_spawn = enabled;
        self
    }

    pub(crate) fn shutdown_handle(&self) -> DispatcherShutdown {
        self.dispatcher.shutdown_handle()
    }

    pub(crate) fn requested_shutdown_deadline(&self) -> Option<Instant> {
        self.dispatcher.requested_shutdown_deadline()
    }

    pub(crate) fn begin_shutdown_at(&self, deadline: Instant) {
        self.dispatcher.begin_shutdown_at(deadline);
    }

    pub(crate) async fn serve(
        &mut self,
        endpoint: &mut ClaimedEndpointV1,
    ) -> anyhow::Result<EndpointCleanupV1> {
        let project_id = endpoint.project_id();
        let daemon_instance_id = endpoint.daemon_instance_id();
        let mut fatal = None;

        let requested_before_serve = *self.shutdown.borrow_and_update();
        let drain_deadline = if let Some(deadline) = requested_before_serve {
            self.dispatcher.begin_draining().await;
            deadline
        } else {
            loop {
                match next_serve_event(
                    &mut self.shutdown,
                    accept_when_capacity(endpoint, &self.connections),
                    &mut self.sessions,
                )
                .await
                {
                    ServeEvent::Shutdown(Ok(Some(deadline))) => {
                        self.dispatcher.begin_draining().await;
                        break deadline;
                    }
                    ServeEvent::Shutdown(Ok(None)) => {}
                    ServeEvent::Shutdown(Err(_)) => {
                        self.dispatcher.begin_draining().await;
                        fatal = Some(anyhow::anyhow!(
                            "IPC shutdown controller closed unexpectedly"
                        ));
                        break fatal_drain_deadline();
                    }
                    ServeEvent::Accepted(accepted) => {
                        let (stream, permit) = match accepted {
                            Ok(accepted) => accepted,
                            Err(error) => {
                                let requested_deadline = *self.shutdown.borrow();
                                if let Some(deadline) = requested_deadline {
                                    self.dispatcher.begin_draining().await;
                                    break deadline;
                                }
                                if error.is_peer_rejection() {
                                    self.rejection_log.record(&error);
                                    continue;
                                }
                                self.dispatcher.begin_draining().await;
                                fatal = Some(anyhow::Error::new(error));
                                break fatal_drain_deadline();
                            }
                        };
                        let dispatcher = self.dispatcher.clone();
                        let dispatch_capacity = self.dispatch_capacity.clone();
                        self.sessions.spawn(async move {
                            if let Err(error) = session(
                                stream,
                                dispatcher,
                                dispatch_capacity,
                                permit,
                                project_id,
                                daemon_instance_id,
                            )
                            .await
                            {
                                eprintln!("local IPC session closed: {error}");
                            }
                        });
                        #[cfg(test)]
                        if self.panic_after_session_spawn {
                            panic!("test-injected IPC service panic after session spawn");
                        }
                    }
                    ServeEvent::SessionJoined(joined) => {
                        if let Err(error) = joined {
                            eprintln!("local IPC session task failed: {error}");
                        }
                    }
                }
            }
        };

        // Stop discovery first, then withdraw the volatile Windows slot and close the listener.
        // Active sessions are already bound to the verified process and may drain within the
        // requested limit.
        let cleanup = endpoint.withdraw();
        self.drain_to(drain_deadline).await;

        match (fatal, cleanup) {
            (None, Ok(cleanup)) => Ok(cleanup),
            (Some(error), Ok(_)) => Err(error),
            (None, Err(error)) => Err(error.into()),
            (Some(fatal), Err(cleanup)) => Err(anyhow::anyhow!(
                "{fatal}; endpoint publication cleanup also failed: {cleanup}"
            )),
        }
    }

    /// Drain every owned session after the supervisor has closed discovery and admission.
    pub(crate) async fn shutdown(&mut self) {
        self.dispatcher.begin_draining().await;
        let deadline = (*self.shutdown.borrow_and_update()).unwrap_or_else(fatal_drain_deadline);
        self.drain_to(deadline).await;
    }

    async fn drain_to(&mut self, deadline: Instant) {
        drain_sessions(&mut self.sessions, &mut self.shutdown, deadline).await;
    }
}

enum ServeEvent<T> {
    Shutdown(Result<Option<Instant>, watch::error::RecvError>),
    Accepted(T),
    SessionJoined(Result<(), tokio::task::JoinError>),
}

async fn next_serve_event<T, A>(
    shutdown: &mut watch::Receiver<Option<Instant>>,
    accepted: A,
    sessions: &mut JoinSet<()>,
) -> ServeEvent<T>
where
    A: Future<Output = T>,
{
    let has_sessions = !sessions.is_empty();
    tokio::select! {
        // Discovery withdrawal must not be starved by a continuously ready listener.
        biased;
        changed = shutdown.changed() => {
            ServeEvent::Shutdown(changed.map(|()| *shutdown.borrow_and_update()))
        }
        joined = sessions.join_next(), if has_sessions => {
            ServeEvent::SessionJoined(
                joined.expect("a non-empty session set yields a completion"),
            )
        }
        accepted = accepted => {
            match *shutdown.borrow_and_update() {
                Some(deadline) => ServeEvent::Shutdown(Ok(Some(deadline))),
                None => ServeEvent::Accepted(accepted),
            }
        }
    }
}

#[cfg(test)]
pub async fn serve(
    endpoint: &mut ClaimedEndpointV1,
    dispatcher: Dispatcher,
) -> anyhow::Result<EndpointCleanupV1> {
    IpcService::new(dispatcher).serve(endpoint).await
}

fn fatal_drain_deadline() -> Instant {
    Instant::now()
        .checked_add(FATAL_DRAIN_TIMEOUT)
        .expect("fatal drain timeout fits Tokio Instant")
}

async fn drain_sessions(
    sessions: &mut JoinSet<()>,
    shutdown: &mut watch::Receiver<Option<Instant>>,
    mut deadline: Instant,
) {
    let mut observe_tightening = true;
    loop {
        if sessions.is_empty() {
            return;
        }
        if deadline <= Instant::now() {
            break;
        }
        tokio::select! {
            changed = shutdown.changed(), if observe_tightening => {
                if changed.is_err() {
                    observe_tightening = false;
                } else if let Some(requested) = *shutdown.borrow_and_update()
                    && requested < deadline
                {
                    deadline = requested;
                }
            }
            joined = sessions.join_next() => {
                if let Some(Err(error)) = joined {
                    eprintln!("local IPC session task failed during shutdown: {error}");
                }
            }
            () = tokio::time::sleep_until(deadline) => break,
        }
    }
    sessions.abort_all();
    while sessions.join_next().await.is_some() {}
}

struct PeerRejectionLog {
    next_report: StdInstant,
    suppressed: u64,
}

impl PeerRejectionLog {
    fn new() -> Self {
        Self {
            next_report: StdInstant::now(),
            suppressed: 0,
        }
    }

    fn record(&mut self, error: &EndpointTransportError) {
        self.suppressed = self.suppressed.saturating_add(1);
        let now = StdInstant::now();
        if now >= self.next_report {
            eprintln!(
                "local IPC peer rejected ({} attempt(s) since last report): {error}",
                self.suppressed
            );
            self.suppressed = 0;
            self.next_report = now + Duration::from_secs(1);
        }
    }
}

async fn accept_when_capacity(
    endpoint: &mut ClaimedEndpointV1,
    connections: &ConnectionCapacity,
) -> Result<(VerifiedLocalStreamV1, SessionLease), EndpointTransportError> {
    let lease = connections.acquire().await;
    // The platform listener remains armed while capacity is unavailable. The Windows transport
    // retains exactly one single-use pending slot until ownership transfers into this future.
    let stream = endpoint.accept_verified().await?;
    Ok((stream, lease))
}

async fn session(
    mut stream: VerifiedLocalStreamV1,
    dispatcher: Dispatcher,
    dispatch_capacity: DispatchCapacity,
    connection: SessionLease,
    project_id: unity_asset_search_protocol::ProjectId,
    daemon_instance_id: unity_asset_search_protocol::DaemonInstanceId,
) -> Result<(), SessionError> {
    let lane = connection.lane();
    let _connection = connection;
    let expected_context = stream.peer_identity().security_context_id();
    let bootstrap_frame = match read_frame(
        &mut stream,
        FrameLimits::bootstrap().max_encoded_bytes(),
        lane.bootstrap_timeout(),
    )
    .await
    {
        Err(SessionError::ReadTimeout) if lane.single_request() => return Ok(()),
        result => result?,
    }
    .ok_or(SessionError::ClosedDuringBootstrap)?;
    stream.verify_received_message_principal(expected_context)?;
    let mut budget = AssetLoadBudget::default();
    let hello: BootstrapHelloV2 =
        decode_validated_frame(&bootstrap_frame, &mut budget, FrameLimits::bootstrap())?;
    let reply = BootstrapReplyV2::negotiate(
        &hello,
        project_id,
        daemon_instance_id,
        dispatcher.query_policy_id(),
        &[BUSINESS_PROTOCOL_REVISION],
    );
    let reply_frame = encode_frame(&reply, FrameLimits::bootstrap())?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    match write_frame_monitoring_pipeline(&mut reader, &mut writer, &reply_frame, PeerState::Open)
        .await?
    {
        PeerState::Open => stream = reader.unsplit(writer),
        PeerState::Closed => return Ok(()),
        PeerState::Pipelined => return Err(SessionError::PipelinedRequest),
    }
    if reply.selected_revision().is_none() {
        return Ok(());
    }

    loop {
        let frame = match read_frame(
            &mut stream,
            FrameLimits::request_envelope().max_encoded_bytes(),
            lane.request_timeout(),
        )
        .await
        {
            Err(SessionError::ReadTimeout) if lane.single_request() => return Ok(()),
            result => result?,
        };
        let Some(frame) = frame else { return Ok(()) };
        stream.verify_received_message_principal(expected_context)?;
        let mut budget = AssetLoadBudget::default();
        let request = decode_request_frame(&frame, &mut budget)?;
        request.validate_binding(project_id, daemon_instance_id, dispatcher.query_policy_id())?;
        let (mut reader, mut writer) = tokio::io::split(stream);
        let operation = request.operation().clone();
        let dispatch_class = DispatchClass::for_operation(operation.kind());
        let request_dispatcher = dispatcher.clone();
        let request_capacity = dispatch_capacity.clone();
        let dispatch = async move {
            if !lane.permits(dispatch_class) {
                return dispatch::DispatchResult {
                    response: Err(ApiError::new(
                        ApiErrorCode::Busy,
                        "control-reserved connection only accepts control operations",
                        true,
                    )
                    .with_detail("lane", "control_reserved")
                    .with_detail("accepted_class", DispatchClass::Control.name())
                    .with_query_policy(request_dispatcher.query_policy_id())),
                    shutdown_after_response: None,
                };
            }
            let permit = request_capacity.try_acquire(dispatch_class);
            match permit {
                Ok(_permit) => request_dispatcher.dispatch(operation).await,
                Err(_) => dispatch::DispatchResult {
                    response: Err(ApiError::new(
                        ApiErrorCode::Busy,
                        "daemon in-flight request class limit reached",
                        true,
                    )
                    .with_detail("class", dispatch_class.name())
                    .with_detail("maximum", dispatch_class.maximum().to_string())
                    .with_query_policy(request_dispatcher.query_policy_id())),
                    shutdown_after_response: None,
                },
            }
        };
        tokio::pin!(dispatch);
        let mut pipeline_byte = [0_u8; 1];
        let (dispatched, peer_state) = tokio::select! {
            biased;
            read = reader.read(&mut pipeline_byte) => {
                let peer_state = PeerState::from_read(read).unwrap_or(PeerState::Closed);
                (dispatch.await, peer_state)
            }
            dispatched = &mut dispatch => (dispatched, PeerState::Open),
        };
        if peer_state == PeerState::Closed {
            if let Some(deadline) = dispatched.shutdown_after_response {
                dispatcher.begin_shutdown_at(deadline);
            }
            return Ok(());
        }
        let response = match dispatched.response {
            Ok(response) => ResponseEnvelope::success(&request, response),
            Err(error) => ResponseEnvelope::error(&request, error),
        };
        let write_result = async {
            let response_frame = encode_response_frame(&response, &request)?;
            write_frame_monitoring_pipeline(&mut reader, &mut writer, &response_frame, peer_state)
                .await
        }
        .await;
        if let Some(deadline) = dispatched.shutdown_after_response {
            dispatcher.begin_shutdown_at(deadline);
        }
        let peer_state = write_result?;
        if peer_state == PeerState::Pipelined {
            return Err(SessionError::PipelinedRequest);
        }
        if dispatched.shutdown_after_response.is_some() {
            return Ok(());
        }
        if lane.single_request() {
            return Ok(());
        }
        stream = reader.unsplit(writer);
    }
}

async fn read_frame(
    stream: &mut VerifiedLocalStreamV1,
    maximum: usize,
    header_timeout: Duration,
) -> Result<Option<Vec<u8>>, SessionError> {
    let mut header = [0_u8; 4];
    let first = tokio::time::timeout(header_timeout, stream.read(&mut header[..1]))
        .await
        .map_err(|_| SessionError::ReadTimeout)??;
    if first == 0 {
        return Ok(None);
    }
    tokio::time::timeout(BODY_TIMEOUT, stream.read_exact(&mut header[1..]))
        .await
        .map_err(|_| SessionError::ReadTimeout)??;
    let declared = u32::from_be_bytes(header) as usize;
    if declared > maximum {
        return Err(SessionError::FrameTooLarge {
            requested: declared,
            maximum,
        });
    }
    let total = declared
        .checked_add(header.len())
        .ok_or(SessionError::FrameLengthOverflow)?;
    let mut frame = Vec::new();
    frame
        .try_reserve_exact(total)
        .map_err(|_| SessionError::AllocationFailed { requested: total })?;
    frame.extend_from_slice(&header);
    frame.resize(total, 0);
    tokio::time::timeout(BODY_TIMEOUT, stream.read_exact(&mut frame[4..]))
        .await
        .map_err(|_| SessionError::ReadTimeout)??;
    Ok(Some(frame))
}

async fn write_frame_monitoring_pipeline<R, W>(
    reader: &mut R,
    writer: &mut W,
    frame: &[u8],
    mut peer_state: PeerState,
) -> Result<PeerState, SessionError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let deadline = tokio::time::Instant::now() + WRITE_TIMEOUT;
    let mut offset = 0;
    let mut pipeline_byte = [0_u8; 1];
    while offset < frame.len() {
        if peer_state == PeerState::Closed {
            return Ok(peer_state);
        }
        if peer_state == PeerState::Open {
            if let Some(observed) = probe_peer_state(reader, &mut pipeline_byte).await? {
                peer_state = observed;
                continue;
            }
            tokio::select! {
                biased;
                written = tokio::time::timeout_at(deadline, writer.write(&frame[offset..])) => {
                    offset = advance_write(offset, written)?;
                }
                read = reader.read(&mut pipeline_byte) => {
                    peer_state = PeerState::from_read(read)?;
                }
            }
        } else {
            offset = advance_write(
                offset,
                tokio::time::timeout_at(deadline, writer.write(&frame[offset..])).await,
            )?;
        }
    }
    if peer_state == PeerState::Closed {
        return Ok(peer_state);
    }
    // Once the final frame byte has been accepted by the OS, a following request belongs to the
    // next sequential exchange. Monitoring reads while flush races would misclassify a client that
    // consumed the complete response and immediately sent its next request.
    tokio::time::timeout_at(deadline, writer.flush())
        .await
        .map_err(|_| SessionError::WriteTimeout)??;
    Ok(peer_state)
}

async fn probe_peer_state<R>(
    reader: &mut R,
    byte: &mut [u8; 1],
) -> Result<Option<PeerState>, SessionError>
where
    R: AsyncRead + Unpin,
{
    std::future::poll_fn(|context| {
        let mut buffer = tokio::io::ReadBuf::new(byte);
        match std::pin::Pin::new(&mut *reader).poll_read(context, &mut buffer) {
            std::task::Poll::Pending => std::task::Poll::Ready(Ok(None)),
            std::task::Poll::Ready(Ok(())) => {
                std::task::Poll::Ready(PeerState::from_read(Ok(buffer.filled().len())).map(Some))
            }
            std::task::Poll::Ready(Err(source)) => {
                std::task::Poll::Ready(Err(SessionError::Io(source)))
            }
        }
    })
    .await
}

fn advance_write(
    offset: usize,
    result: Result<io::Result<usize>, tokio::time::error::Elapsed>,
) -> Result<usize, SessionError> {
    let written = result.map_err(|_| SessionError::WriteTimeout)??;
    if written == 0 {
        return Err(SessionError::Io(io::Error::new(
            io::ErrorKind::WriteZero,
            "local IPC response write returned zero bytes",
        )));
    }
    Ok(offset + written)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerState {
    Open,
    Closed,
    Pipelined,
}

impl PeerState {
    fn from_read(result: io::Result<usize>) -> Result<Self, SessionError> {
        match result {
            Ok(0) => Ok(Self::Closed),
            Ok(_) => Ok(Self::Pipelined),
            Err(source) => Err(SessionError::Io(source)),
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum SessionError {
    #[error("peer closed during bootstrap")]
    ClosedDuringBootstrap,
    #[error("read deadline elapsed")]
    ReadTimeout,
    #[error("write deadline elapsed")]
    WriteTimeout,
    #[error("frame declared {requested} bytes; maximum is {maximum}")]
    FrameTooLarge { requested: usize, maximum: usize },
    #[error("frame length overflow")]
    FrameLengthOverflow,
    #[error("could not allocate {requested} frame bytes")]
    AllocationFailed { requested: usize },
    #[error("client pipelined a second request")]
    PipelinedRequest,
    #[error(transparent)]
    Transport(#[from] EndpointTransportError),
    #[error(transparent)]
    Framing(#[from] unity_asset_search_protocol::FramingError),
    #[error(transparent)]
    Contract(#[from] unity_asset_search_protocol::ContractValidationError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant as StdInstant};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::sync::watch;
    use tokio::time::Instant;
    use unity_asset_core::AssetLoadBudget;
    use unity_asset_search_index::{IndexPaths, SearchIndex, SearchIndexOptions};
    use unity_asset_search_local::{
        EndpointCleanupV1, PrivateRootsV1, VerifiedLocalStreamV1, generate_daemon_instance_id,
    };
    use unity_asset_search_protocol::{
        ApiErrorCode, BUSINESS_PROTOCOL_REVISION, BootstrapHelloV2, BootstrapReplyV2,
        DaemonInstanceId, FilesystemReindexIntent, FrameLimits, OperationId, OperationKind,
        ProjectId, ReindexAdmitRequest, ReindexCancelRequest, ReindexStatusRequest,
        ReindexWaitRequest, RequestEnvelope, RequestId, RequestOperation, ResponseEnvelope,
        ResponseOperation, ResponseOutcome, SearchRequest, ShutdownRequest, StatusRequest,
        decode_response_frame, decode_validated_frame, encode_frame, encode_request_frame,
    };

    use super::{
        CONTROL_RESERVED_CONNECTIONS, ConnectionCapacity, DispatchCapacity, DispatchClass,
        Dispatcher, MAX_LOCAL_IPC_CONNECTIONS_V1, ORDINARY_CONNECTIONS, PeerState, ServeEvent,
        SessionError, SessionLane, drain_sessions, next_serve_event, read_frame, session,
        write_frame_monitoring_pipeline,
    };
    use crate::coordinator::{ReindexCoordinatorConfig, ReindexCoordinatorRuntime};
    use crate::lifecycle::{AdmissionGate, BlockingTaskOwner};
    use crate::watcher::MaintenanceRuntime;

    #[tokio::test]
    async fn sequential_response_completes_without_pipeline_evidence() {
        let (server, mut client) = tokio::io::duplex(128);
        let (mut reader, mut writer) = tokio::io::split(server);
        let response = b"first-response";

        let state =
            write_frame_monitoring_pipeline(&mut reader, &mut writer, response, PeerState::Open)
                .await
                .expect("write response");

        assert_eq!(state, PeerState::Open);
        let mut received = vec![0_u8; response.len()];
        client
            .read_exact(&mut received)
            .await
            .expect("read response");
        assert_eq!(received, response);
    }

    #[tokio::test]
    async fn buffered_second_request_is_detected_while_first_response_completes() {
        let (server, mut client) = tokio::io::duplex(128);
        let (mut reader, mut writer) = tokio::io::split(server);
        let response = b"first-response";
        client.write_all(&[0x00]).await.expect("pipeline one byte");

        let state =
            write_frame_monitoring_pipeline(&mut reader, &mut writer, response, PeerState::Open)
                .await
                .expect("write first response");

        assert_eq!(state, PeerState::Pipelined);
        let mut received = vec![0_u8; response.len()];
        client
            .read_exact(&mut received)
            .await
            .expect("read first response");
        assert_eq!(received, response);
    }

    #[tokio::test]
    async fn closed_peer_stops_response_materialization_without_write_error() {
        let (server, client) = tokio::io::duplex(128);
        let (mut reader, mut writer) = tokio::io::split(server);
        drop(client);

        let state = write_frame_monitoring_pipeline(
            &mut reader,
            &mut writer,
            b"unused-response",
            PeerState::Open,
        )
        .await
        .expect("observe closed peer");

        assert_eq!(state, PeerState::Closed);
    }

    #[tokio::test]
    async fn saturated_long_polls_preserve_control_and_work_capacity() {
        let capacity = DispatchCapacity::new(1, 1, 2);
        let _wait = capacity.try_acquire(DispatchClass::Wait).unwrap();
        assert!(capacity.try_acquire(DispatchClass::Wait).is_err());

        assert_eq!(
            DispatchClass::for_operation(OperationKind::ReindexStatus),
            DispatchClass::Control
        );
        assert_eq!(
            DispatchClass::for_operation(OperationKind::ReindexCancel),
            DispatchClass::Control
        );
        assert_eq!(
            DispatchClass::for_operation(OperationKind::Shutdown),
            DispatchClass::Control
        );
        let _status = capacity.try_acquire(DispatchClass::Control).unwrap();
        let _shutdown = capacity.try_acquire(DispatchClass::Control).unwrap();
        assert!(capacity.try_acquire(DispatchClass::Control).is_err());

        let _work = capacity.try_acquire(DispatchClass::Work).unwrap();
        assert!(capacity.try_acquire(DispatchClass::Work).is_err());
    }

    #[tokio::test]
    async fn ordinary_saturation_preserves_a_reclaimable_control_session_lane() {
        assert_eq!(
            ORDINARY_CONNECTIONS + CONTROL_RESERVED_CONNECTIONS,
            MAX_LOCAL_IPC_CONNECTIONS_V1
        );
        let capacity = ConnectionCapacity::new(1, 1);
        let ordinary = capacity.acquire().await;
        assert_eq!(ordinary.lane(), SessionLane::Ordinary);

        let reserved = capacity.acquire().await;
        assert_eq!(reserved.lane(), SessionLane::ControlReserved);
        assert!(
            reserved
                .lane()
                .permits(DispatchClass::for_operation(OperationKind::Status))
        );
        assert!(
            reserved
                .lane()
                .permits(DispatchClass::for_operation(OperationKind::Shutdown))
        );
        assert!(
            !reserved
                .lane()
                .permits(DispatchClass::for_operation(OperationKind::Search))
        );

        drop(reserved);
        let reclaimed = capacity.acquire().await;
        assert_eq!(reclaimed.lane(), SessionLane::ControlReserved);
        drop(reclaimed);
        drop(ordinary);
    }

    #[tokio::test]
    async fn ready_shutdown_preempts_a_ready_accept() {
        let (shutdown, mut receiver) = watch::channel(None);
        let deadline = Instant::now() + Duration::from_secs(1);
        shutdown.send(Some(deadline)).unwrap();
        let mut sessions = tokio::task::JoinSet::new();

        let event =
            next_serve_event(&mut receiver, std::future::ready("accepted"), &mut sessions).await;

        assert!(matches!(
            event,
            ServeEvent::Shutdown(Ok(Some(observed))) if observed == deadline
        ));
    }

    #[tokio::test]
    async fn shutdown_arriving_during_accept_discards_the_accepted_value() {
        let (shutdown, mut receiver) = watch::channel(None);
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut sessions = tokio::task::JoinSet::new();

        let event = next_serve_event(
            &mut receiver,
            async move {
                shutdown.send(Some(deadline)).unwrap();
                "accepted"
            },
            &mut sessions,
        )
        .await;

        assert!(matches!(
            event,
            ServeEvent::Shutdown(Ok(Some(observed))) if observed == deadline
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn zero_shutdown_drain_aborts_sessions_without_waiting() {
        let mut sessions = tokio::task::JoinSet::new();
        sessions.spawn(std::future::pending::<()>());
        let (_shutdown, mut receiver) = watch::channel(None);

        drain_sessions(&mut sessions, &mut receiver, Instant::now()).await;

        assert!(sessions.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_shutdown_drain_aborts_after_requested_timeout() {
        let mut sessions = tokio::task::JoinSet::new();
        sessions.spawn(std::future::pending::<()>());
        let (_shutdown, mut receiver) = watch::channel(None);

        drain_sessions(
            &mut sessions,
            &mut receiver,
            Instant::now() + Duration::from_millis(5),
        )
        .await;

        assert!(sessions.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn active_shutdown_drain_observes_a_tighter_deadline() {
        let mut sessions = tokio::task::JoinSet::new();
        sessions.spawn(std::future::pending::<()>());
        let (shutdown, mut receiver) = watch::channel(None);
        let initial = Instant::now() + Duration::from_secs(60);
        let drain = tokio::spawn(async move {
            drain_sessions(&mut sessions, &mut receiver, initial).await;
            sessions
        });
        tokio::task::yield_now().await;

        shutdown.send(Some(Instant::now())).unwrap();
        let sessions = drain.await.unwrap();

        assert!(sessions.is_empty());
        assert!(Instant::now() < initial);
    }

    #[derive(Clone, Copy)]
    enum SessionCase {
        Bootstrap,
        PipelinedBusiness,
        SequentialBusiness,
        ReservedWork,
    }

    async fn bootstrap_client(
        client: &mut VerifiedLocalStreamV1,
        project_id: ProjectId,
        instance_id: DaemonInstanceId,
    ) {
        let hello =
            BootstrapHelloV2::new(project_id, instance_id, vec![BUSINESS_PROTOCOL_REVISION])
                .unwrap();
        client
            .write_all(&encode_frame(&hello, FrameLimits::bootstrap()).unwrap())
            .await
            .unwrap();
        let reply_frame = read_frame(
            client,
            FrameLimits::bootstrap().max_encoded_bytes(),
            Duration::from_secs(5),
        )
        .await
        .unwrap()
        .unwrap();
        let mut budget = AssetLoadBudget::default();
        let reply: BootstrapReplyV2 =
            decode_validated_frame(&reply_frame, &mut budget, FrameLimits::bootstrap()).unwrap();
        assert_eq!(reply.selected_revision(), Some(BUSINESS_PROTOCOL_REVISION));
    }

    async fn exchange_request(
        client: &mut VerifiedLocalStreamV1,
        request: &RequestEnvelope,
    ) -> ResponseEnvelope {
        client
            .write_all(&encode_request_frame(request).unwrap())
            .await
            .unwrap();
        let response_frame = read_frame(
            client,
            FrameLimits::response(request.operation().kind()).max_encoded_bytes(),
            Duration::from_secs(5),
        )
        .await
        .unwrap()
        .unwrap();
        let mut budget = AssetLoadBudget::default();
        decode_response_frame(&response_frame, &mut budget, request).unwrap()
    }

    async fn run_session_case(case: SessionCase) -> u64 {
        let project = crate::secure_test_tempdir();
        std::fs::create_dir(project.path().join("Assets")).unwrap();
        std::fs::create_dir(project.path().join("ProjectSettings")).unwrap();
        let index_root = project.path().join("index");
        let paths =
            IndexPaths::for_project(project.path().to_path_buf(), Some(index_root), None).unwrap();
        let mut budget = AssetLoadBudget::default();
        let index = SearchIndex::open_or_create_with_options(
            paths,
            SearchIndexOptions::default(),
            &mut budget,
        )
        .unwrap();
        let _coordinator_runtime = ReindexCoordinatorRuntime::start(
            ReindexCoordinatorConfig::new(project.path().to_path_buf())
                .with_debounce(Duration::from_secs(60))
                .with_max_debounce(Duration::from_secs(60)),
            |_intent| async move { std::future::pending().await },
        )
        .unwrap();
        let coordinator = _coordinator_runtime.coordinator();

        let roots = PrivateRootsV1::discover_for_current_context().unwrap();
        let mut project_bytes = rand::random::<[u8; 32]>();
        project_bytes[0] |= 1;
        let project_id = ProjectId::from_bytes(project_bytes);
        let namespace = roots.runtime().endpoint_namespace(project_id).unwrap();
        let cleanup_path = namespace.path().to_path_buf();
        let mut claim = namespace.claim_daemon_endpoint().unwrap();
        let instance_id = generate_daemon_instance_id().unwrap();
        let mut endpoint = claim.publish(instance_id).unwrap();
        let discovered = namespace.discover_endpoint().unwrap();
        let (accepted, connected) = tokio::join!(
            endpoint.accept_verified(),
            discovered.connect_verified(&namespace, StdInstant::now() + Duration::from_secs(5))
        );
        let accepted = accepted.unwrap();
        let mut connected = connected.unwrap();
        let _blocking_tasks = BlockingTaskOwner::new();
        let query_policy_id = index.status().unwrap().query_policy_id;
        let lifecycle_admission = AdmissionGate::default();
        let operation_registry = super::OperationRegistryOwner::new(
            instance_id,
            coordinator.clone(),
            project.path().to_path_buf(),
            query_policy_id,
            lifecycle_admission.clone(),
        );
        let maintenance = MaintenanceRuntime::start(operation_registry.registry(), None, None);
        let dispatcher = Dispatcher::new(
            index,
            _blocking_tasks.handle(),
            operation_registry.registry(),
            lifecycle_admission,
            maintenance.handle(),
        );
        let connection =
            ConnectionCapacity::new(usize::from(!matches!(case, SessionCase::ReservedWork)), 1)
                .acquire()
                .await;
        let server_session = tokio::spawn(session(
            accepted,
            dispatcher.clone(),
            DispatchCapacity::new(1, 1, 1),
            connection,
            project_id,
            instance_id,
        ));

        let hello =
            BootstrapHelloV2::new(project_id, instance_id, vec![BUSINESS_PROTOCOL_REVISION])
                .unwrap();
        let hello_frame = encode_frame(&hello, FrameLimits::bootstrap()).unwrap();
        let first_request = RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([7; 16]),
            project_id,
            instance_id,
            dispatcher.query_policy_id(),
            RequestOperation::ReindexAdmit(ReindexAdmitRequest {
                intent: FilesystemReindexIntent::full(),
                idempotency_key: None,
            }),
        )
        .unwrap();
        let first_frame = encode_request_frame(&first_request).unwrap();

        if matches!(case, SessionCase::Bootstrap) {
            let mut pipelined = hello_frame;
            pipelined.try_reserve(first_frame.len()).unwrap();
            pipelined.extend_from_slice(&first_frame);
            connected.write_all(&pipelined).await.unwrap();
        } else {
            connected.write_all(&hello_frame).await.unwrap();
        }

        let reply_frame = read_frame(
            &mut connected,
            FrameLimits::bootstrap().max_encoded_bytes(),
            Duration::from_secs(5),
        )
        .await
        .unwrap()
        .unwrap();
        let mut reply_budget = AssetLoadBudget::default();
        let reply: BootstrapReplyV2 =
            decode_validated_frame(&reply_frame, &mut reply_budget, FrameLimits::bootstrap())
                .unwrap();
        assert_eq!(reply.selected_revision(), Some(BUSINESS_PROTOCOL_REVISION));

        if !matches!(case, SessionCase::Bootstrap) {
            let second_request = RequestEnvelope::new(
                BUSINESS_PROTOCOL_REVISION,
                RequestId::from_bytes([8; 16]),
                project_id,
                instance_id,
                dispatcher.query_policy_id(),
                RequestOperation::ReindexAdmit(ReindexAdmitRequest {
                    intent: FilesystemReindexIntent::reconcile(),
                    idempotency_key: None,
                }),
            )
            .unwrap();
            let second_frame = encode_request_frame(&second_request).unwrap();
            if matches!(case, SessionCase::PipelinedBusiness) {
                let mut pipelined = first_frame;
                pipelined.try_reserve(second_frame.len()).unwrap();
                pipelined.extend_from_slice(&second_frame);
                connected.write_all(&pipelined).await.unwrap();
                assert!(
                    read_frame(
                        &mut connected,
                        FrameLimits::response(OperationKind::ReindexAdmit).max_encoded_bytes(),
                        Duration::from_secs(5),
                    )
                    .await
                    .unwrap()
                    .is_some()
                );
            } else {
                connected.write_all(&first_frame).await.unwrap();
                let response_frame = read_frame(
                    &mut connected,
                    FrameLimits::response(OperationKind::ReindexAdmit).max_encoded_bytes(),
                    Duration::from_secs(5),
                )
                .await
                .unwrap()
                .unwrap();
                if matches!(case, SessionCase::ReservedWork) {
                    let mut budget = AssetLoadBudget::default();
                    let response =
                        decode_response_frame(&response_frame, &mut budget, &first_request)
                            .unwrap();
                    let ResponseOutcome::Error(error) = response.into_outcome() else {
                        panic!("reserved work request must return a structured error");
                    };
                    assert_eq!(error.code, ApiErrorCode::Busy);
                    assert_eq!(
                        error.details.get("lane").map(String::as_str),
                        Some("control_reserved")
                    );
                } else {
                    connected.write_all(&second_frame).await.unwrap();
                    assert!(
                        read_frame(
                            &mut connected,
                            FrameLimits::response(OperationKind::ReindexAdmit).max_encoded_bytes(),
                            Duration::from_secs(5),
                        )
                        .await
                        .unwrap()
                        .is_some()
                    );
                    connected.shutdown().await.unwrap();
                }
            }
        }

        drop(connected);
        let session_result = server_session.await.unwrap();
        if matches!(
            case,
            SessionCase::SequentialBusiness | SessionCase::ReservedWork
        ) {
            assert!(session_result.is_ok());
        } else {
            assert!(matches!(
                session_result,
                Err(SessionError::PipelinedRequest)
            ));
        }
        let admissions = coordinator.snapshot().await.admissions.ipc;

        assert_eq!(endpoint.withdraw().unwrap(), EndpointCleanupV1::Removed);
        drop(endpoint);
        drop(namespace);
        drop(roots);
        for name in ["binding.v1", ".binding-v1.lock", ".daemon-v1.lock"] {
            let result = std::fs::remove_file(cleanup_path.join(name));
            assert!(
                result.is_ok()
                    || result.is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            );
        }
        std::fs::remove_dir(cleanup_path).unwrap();
        admissions
    }

    #[tokio::test]
    async fn business_frame_pipelined_with_bootstrap_never_reaches_dispatch() {
        assert_eq!(run_session_case(SessionCase::Bootstrap).await, 0);
    }

    #[tokio::test]
    async fn second_business_frame_never_reaches_dispatch() {
        assert_eq!(run_session_case(SessionCase::PipelinedBusiness).await, 1);
    }

    #[tokio::test]
    async fn sequential_business_frames_reuse_one_connection() {
        assert_eq!(run_session_case(SessionCase::SequentialBusiness).await, 2);
    }

    #[tokio::test]
    async fn reserved_session_rejects_work_before_dispatch() {
        assert_eq!(run_session_case(SessionCase::ReservedWork).await, 0);
    }

    #[tokio::test]
    async fn established_session_cannot_admit_work_after_peer_requests_shutdown() {
        let project = crate::secure_test_tempdir();
        std::fs::create_dir(project.path().join("Assets")).unwrap();
        std::fs::create_dir(project.path().join("ProjectSettings")).unwrap();
        let paths = IndexPaths::for_project(
            project.path().to_path_buf(),
            Some(project.path().join("index")),
            None,
        )
        .unwrap();
        let mut budget = AssetLoadBudget::default();
        let index = SearchIndex::open_or_create_with_options(
            paths,
            SearchIndexOptions::default(),
            &mut budget,
        )
        .unwrap();
        let mut coordinator_runtime = ReindexCoordinatorRuntime::start(
            ReindexCoordinatorConfig::new(project.path().to_path_buf())
                .with_debounce(Duration::from_secs(60))
                .with_max_debounce(Duration::from_secs(60)),
            |_intent| async move { std::future::pending().await },
        )
        .unwrap();
        let coordinator = coordinator_runtime.coordinator();

        let roots = PrivateRootsV1::discover_for_current_context().unwrap();
        let mut project_bytes = rand::random::<[u8; 32]>();
        project_bytes[0] |= 1;
        let project_id = ProjectId::from_bytes(project_bytes);
        let namespace = roots.runtime().endpoint_namespace(project_id).unwrap();
        let cleanup_path = namespace.path().to_path_buf();
        let mut claim = namespace.claim_daemon_endpoint().unwrap();
        let instance_id = generate_daemon_instance_id().unwrap();
        let endpoint = claim.publish(instance_id).unwrap();

        let mut blocking_tasks = BlockingTaskOwner::new();
        let query_policy_id = index.status().unwrap().query_policy_id;
        let lifecycle_admission = AdmissionGate::default();
        let mut operation_registry = super::OperationRegistryOwner::new(
            instance_id,
            coordinator.clone(),
            project.path().to_path_buf(),
            query_policy_id,
            lifecycle_admission.clone(),
        );
        let mut maintenance = MaintenanceRuntime::start(operation_registry.registry(), None, None);
        let dispatcher = Dispatcher::new(
            index,
            blocking_tasks.handle(),
            operation_registry.registry(),
            lifecycle_admission,
            maintenance.handle(),
        );
        let server = tokio::spawn(async move {
            let mut endpoint = endpoint;
            let result = super::serve(&mut endpoint, dispatcher).await;
            (result, endpoint)
        });

        let first_descriptor = namespace.discover_endpoint().unwrap();
        let mut shutdown_client = first_descriptor
            .connect_verified(&namespace, StdInstant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        bootstrap_client(&mut shutdown_client, project_id, instance_id).await;
        let second_descriptor = namespace.discover_endpoint().unwrap();
        let mut work_client = second_descriptor
            .connect_verified(&namespace, StdInstant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        bootstrap_client(&mut work_client, project_id, instance_id).await;

        let shutdown = RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([21; 16]),
            project_id,
            instance_id,
            query_policy_id,
            RequestOperation::Shutdown(ShutdownRequest {
                drain_timeout_ms: 5_000,
            }),
        )
        .unwrap();
        assert!(matches!(
            exchange_request(&mut shutdown_client, &shutdown)
                .await
                .into_outcome(),
            ResponseOutcome::Success(_)
        ));

        let status = RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([22; 16]),
            project_id,
            instance_id,
            query_policy_id,
            RequestOperation::Status(StatusRequest::default()),
        )
        .unwrap();
        assert!(matches!(
            exchange_request(&mut work_client, &status)
                .await
                .into_outcome(),
            ResponseOutcome::Success(response)
                if matches!(response.as_ref(), ResponseOperation::Status(_))
        ));

        let mut unknown_operation_bytes = [0x77; 16];
        unknown_operation_bytes[..8].copy_from_slice(&instance_id.as_bytes()[..8]);
        unknown_operation_bytes[8] |= 1;
        let unknown_operation = OperationId::from_bytes(unknown_operation_bytes);
        let operation_status = RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([23; 16]),
            project_id,
            instance_id,
            query_policy_id,
            RequestOperation::ReindexStatus(ReindexStatusRequest {
                operation_id: unknown_operation,
            }),
        )
        .unwrap();
        let ResponseOutcome::Error(error) = exchange_request(&mut work_client, &operation_status)
            .await
            .into_outcome()
        else {
            panic!("unknown draining operation unexpectedly had retained status");
        };
        assert_eq!(error.code, ApiErrorCode::OperationNotFound);

        let admissions_before = coordinator.snapshot().await.admissions.ipc;
        let reindex = RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([24; 16]),
            project_id,
            instance_id,
            query_policy_id,
            RequestOperation::ReindexAdmit(ReindexAdmitRequest {
                intent: FilesystemReindexIntent::full(),
                idempotency_key: None,
            }),
        )
        .unwrap();
        let ResponseOutcome::Error(error) = exchange_request(&mut work_client, &reindex)
            .await
            .into_outcome()
        else {
            panic!("draining daemon admitted reindex work");
        };
        assert_eq!(error.code, ApiErrorCode::NotReady);

        let search = RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([25; 16]),
            project_id,
            instance_id,
            query_policy_id,
            RequestOperation::Search(SearchRequest {
                query: "player".to_owned(),
                limit: 1,
            }),
        )
        .unwrap();
        let ResponseOutcome::Error(error) = exchange_request(&mut work_client, &search)
            .await
            .into_outcome()
        else {
            panic!("draining daemon executed a search");
        };
        assert_eq!(error.code, ApiErrorCode::NotReady);

        for (request_id, operation) in [
            (
                26,
                RequestOperation::ReindexWait(ReindexWaitRequest {
                    operation_id: unknown_operation,
                    timeout_ms: 1,
                }),
            ),
            (
                27,
                RequestOperation::ReindexCancel(ReindexCancelRequest {
                    operation_id: unknown_operation,
                }),
            ),
        ] {
            let request = RequestEnvelope::new(
                BUSINESS_PROTOCOL_REVISION,
                RequestId::from_bytes([request_id; 16]),
                project_id,
                instance_id,
                query_policy_id,
                operation,
            )
            .unwrap();
            let ResponseOutcome::Error(error) = exchange_request(&mut work_client, &request)
                .await
                .into_outcome()
            else {
                panic!("draining daemon admitted operation state mutation");
            };
            assert_eq!(error.code, ApiErrorCode::NotReady);
        }
        assert_eq!(
            coordinator.snapshot().await.admissions.ipc,
            admissions_before
        );

        shutdown_client.shutdown().await.unwrap();
        work_client.shutdown().await.unwrap();
        drop(shutdown_client);
        drop(work_client);
        let (serve_result, endpoint) = server.await.unwrap();
        assert_eq!(serve_result.unwrap(), EndpointCleanupV1::Removed);
        maintenance.shutdown().await.unwrap();
        coordinator_runtime.shutdown().await.unwrap();
        operation_registry.shutdown().await.unwrap();
        blocking_tasks.shutdown().await.unwrap();
        drop(endpoint);
        drop(namespace);
        drop(roots);
        for name in ["binding.v1", ".binding-v1.lock", ".daemon-v1.lock"] {
            let result = std::fs::remove_file(cleanup_path.join(name));
            assert!(
                result.is_ok()
                    || result.is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            );
        }
        std::fs::remove_dir(cleanup_path).unwrap();
    }
}
