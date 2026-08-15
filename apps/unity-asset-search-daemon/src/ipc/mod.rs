use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

#[cfg(test)]
use tokio::sync::oneshot;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use unity_asset_core::AssetLoadBudget;
use unity_asset_search_local::{
    ClaimedEndpointV1, EndpointCleanupV1, EndpointTransportError, FrameReadTimeoutsV1,
    FramedPeerStateV1, MAX_LOCAL_IPC_CONNECTIONS_V1, VerifiedFramedTransportV1,
};
use unity_asset_search_protocol::{
    ApiError, ApiErrorCode, BUSINESS_PROTOCOL_REVISION, BootstrapHelloV2, BootstrapReplyV2,
    FrameLimits, RequestEnvelope, ResponseEnvelope, decode_request_frame, decode_validated_frame,
    encode_frame, encode_response_frame,
};

use crate::service::{RequestClass, SearchService, SearchServiceResult, SearchServiceShutdown};

const UNCLASSIFIED_CONNECTION_HEADROOM: usize = 8;
const CONTROL_RESERVED_CONNECTIONS: usize = 8;
const ORDINARY_CONNECTIONS: usize =
    MAX_LOCAL_IPC_CONNECTIONS_V1 - UNCLASSIFIED_CONNECTION_HEADROOM - CONTROL_RESERVED_CONNECTIONS;
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(5);
// One budget spans bootstrap, first-request classification, control admission, dispatch, and the
// complete control response write. Unclassified peers therefore never consume reserved capacity,
// and a non-reading control peer cannot extend its lease with a fresh write timeout.
const PRECLASSIFICATION_AND_CONTROL_TIMEOUT: Duration = Duration::from_secs(4);
const CONTROL_ADMISSION_RESPONSE_RESERVE: Duration = Duration::from_millis(500);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
const FATAL_DRAIN_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct ConnectionCapacity {
    // Every accepted socket retains one total permit. Classified lane permits are additional
    // logical capabilities, so unused persistent-session capacity can absorb malformed peers
    // without reducing the eight-connection headroom at full classified load.
    total: Arc<Semaphore>,
    ordinary: Arc<Semaphore>,
    control_reserved: Arc<Semaphore>,
}

impl ConnectionCapacity {
    fn production() -> Self {
        Self::new(
            MAX_LOCAL_IPC_CONNECTIONS_V1,
            ORDINARY_CONNECTIONS,
            CONTROL_RESERVED_CONNECTIONS,
        )
    }

    fn new(total: usize, ordinary: usize, control_reserved: usize) -> Self {
        Self {
            total: Arc::new(Semaphore::new(total)),
            ordinary: Arc::new(Semaphore::new(ordinary)),
            control_reserved: Arc::new(Semaphore::new(control_reserved)),
        }
    }

    async fn acquire_connection(&self) -> ConnectionLease {
        ConnectionLease {
            _permit: Arc::clone(&self.total)
                .acquire_owned()
                .await
                .expect("global connection semaphore remains open"),
        }
    }

    async fn admit_classified(
        &self,
        class: RequestClass,
        deadline: SessionDeadline,
    ) -> Option<SessionLease> {
        if class != RequestClass::Control {
            return Arc::clone(&self.ordinary)
                .try_acquire_owned()
                .ok()
                .map(|permit| SessionLease::new(SessionCapacityLane::Ordinary, permit));
        }
        if let Ok(permit) = Arc::clone(&self.ordinary).try_acquire_owned() {
            return Some(SessionLease::new(SessionCapacityLane::Ordinary, permit));
        }
        if let Ok(permit) = Arc::clone(&self.control_reserved).try_acquire_owned() {
            return Some(SessionLease::new(
                SessionCapacityLane::ControlReserved,
                permit,
            ));
        }

        let ordinary = Arc::clone(&self.ordinary);
        let control_reserved = Arc::clone(&self.control_reserved);
        deadline
            .reserving_tail(CONTROL_ADMISSION_RESPONSE_RESERVE)
            .run(async move {
                tokio::select! {
                    permit = ordinary.acquire_owned() => SessionLease::new(
                        SessionCapacityLane::Ordinary,
                        permit.expect("ordinary connection semaphore remains open"),
                    ),
                    permit = control_reserved.acquire_owned() => SessionLease::new(
                        SessionCapacityLane::ControlReserved,
                        permit.expect("control-reserved connection semaphore remains open"),
                    ),
                }
            })
            .await
    }
}

struct ConnectionLease {
    _permit: OwnedSemaphorePermit,
}

struct SessionLease {
    capacity_lane: SessionCapacityLane,
    _permit: OwnedSemaphorePermit,
}

impl SessionLease {
    fn new(capacity_lane: SessionCapacityLane, permit: OwnedSemaphorePermit) -> Self {
        Self {
            capacity_lane,
            _permit: permit,
        }
    }

    const fn capacity_lane(&self) -> SessionCapacityLane {
        self.capacity_lane
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionCapacityLane {
    Ordinary,
    ControlReserved,
}

impl SessionCapacityLane {
    const fn permits(self, class: RequestClass) -> bool {
        matches!(self, Self::Ordinary) || matches!(class, RequestClass::Control)
    }

    const fn absolute_deadline(self, initial: SessionDeadline) -> Option<SessionDeadline> {
        match self {
            Self::Ordinary => None,
            Self::ControlReserved => Some(initial),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SessionDeadline {
    at: Instant,
}

impl SessionDeadline {
    fn new() -> Result<Self, EndpointTransportError> {
        Instant::now()
            .checked_add(PRECLASSIFICATION_AND_CONTROL_TIMEOUT)
            .map(|at| Self { at })
            .ok_or(EndpointTransportError::FrameDeadlineOverflow)
    }

    async fn run<F, T>(self, future: F) -> Option<T>
    where
        F: Future<Output = T>,
    {
        tokio::time::timeout_at(self.at, future).await.ok()
    }

    fn expired(self) -> bool {
        Instant::now() >= self.at
    }

    fn reserving_tail(self, tail: Duration) -> Self {
        Self {
            at: self.at.checked_sub(tail).unwrap_or(self.at),
        }
    }
}

/// Process-lifetime owner for accepted local IPC sessions.
///
/// This object deliberately outlives the serving future. If that future panics, the supervisor
/// still owns its `JoinSet` and can explicitly abort and join each session before releasing the
/// endpoint or index-writer leases.
pub(crate) struct IpcService {
    service: SearchService,
    connections: ConnectionCapacity,
    shutdown: watch::Receiver<Option<Instant>>,
    sessions: JoinSet<()>,
    rejection_log: PeerRejectionLog,
    #[cfg(test)]
    session_panic_gate: Option<SessionPanicTestGate>,
    #[cfg(test)]
    sessions_drained: Option<oneshot::Sender<()>>,
}

#[cfg(test)]
pub(crate) struct SessionPanicTestGate {
    pub(crate) spawned: oneshot::Sender<()>,
    pub(crate) release: oneshot::Receiver<()>,
    pub(crate) drained: oneshot::Sender<()>,
}

impl IpcService {
    pub(crate) fn new(service: SearchService) -> Self {
        Self {
            shutdown: service.subscribe_shutdown(),
            service,
            connections: ConnectionCapacity::production(),
            sessions: JoinSet::new(),
            rejection_log: PeerRejectionLog::new(),
            #[cfg(test)]
            session_panic_gate: None,
            #[cfg(test)]
            sessions_drained: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_session_panic_gate(mut self, gate: Option<SessionPanicTestGate>) -> Self {
        self.session_panic_gate = gate;
        self
    }

    pub(crate) fn shutdown_handle(&self) -> SearchServiceShutdown {
        self.service.shutdown_handle()
    }

    pub(crate) fn requested_shutdown_deadline(&self) -> Option<Instant> {
        self.service.requested_shutdown_deadline()
    }

    pub(crate) fn begin_shutdown_at(&self, deadline: Instant) {
        self.service.begin_shutdown_at(deadline);
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
            self.service.begin_draining().await;
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
                        self.service.begin_draining().await;
                        break deadline;
                    }
                    ServeEvent::Shutdown(Ok(None)) => {}
                    ServeEvent::Shutdown(Err(_)) => {
                        self.service.begin_draining().await;
                        fatal = Some(anyhow::anyhow!(
                            "IPC shutdown controller closed unexpectedly"
                        ));
                        break fatal_drain_deadline();
                    }
                    ServeEvent::Accepted(accepted) => {
                        let (stream, connection) = match accepted {
                            Ok(accepted) => accepted,
                            Err(error) => {
                                let requested_deadline = *self.shutdown.borrow();
                                if let Some(deadline) = requested_deadline {
                                    self.service.begin_draining().await;
                                    break deadline;
                                }
                                if error.is_peer_rejection() {
                                    self.rejection_log.record(&error);
                                    continue;
                                }
                                self.service.begin_draining().await;
                                fatal = Some(anyhow::Error::new(error));
                                break fatal_drain_deadline();
                            }
                        };
                        let service = self.service.clone();
                        let connections = self.connections.clone();
                        self.sessions.spawn(async move {
                            if let Err(error) = session(
                                stream,
                                service,
                                connections,
                                connection,
                                project_id,
                                daemon_instance_id,
                            )
                            .await
                            {
                                eprintln!("local IPC session closed: {error}");
                            }
                        });
                        #[cfg(test)]
                        if let Some(gate) = self.session_panic_gate.take() {
                            let SessionPanicTestGate {
                                spawned,
                                release,
                                drained,
                            } = gate;
                            self.sessions_drained = Some(drained);
                            let _ = spawned.send(());
                            let _ = release.await;
                            panic!("test-injected IPC service panic after session spawn");
                        }
                    }
                    ServeEvent::SessionJoined(joined) => {
                        if let Err(error) = joined {
                            let message = crate::truncate_utf8(error.to_string(), 4 * 1024);
                            self.service.begin_draining().await;
                            fatal =
                                Some(anyhow::anyhow!("local IPC session task failed: {message}"));
                            break fatal_drain_deadline();
                        }
                    }
                }
            }
        };

        // Stop discovery first, then withdraw the volatile Windows slot and close the listener.
        // Active sessions are already bound to the verified process and may drain within the
        // requested limit.
        let cleanup = endpoint.withdraw();
        if let Some(message) = self.drain_to(drain_deadline).await {
            fatal = Some(match fatal {
                Some(existing) => anyhow::anyhow!(
                    "{existing}; local IPC session task also failed while draining: {message}"
                ),
                None => anyhow::anyhow!("local IPC session task failed while draining: {message}"),
            });
        }

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
    pub(crate) async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.service.begin_draining().await;
        let deadline = (*self.shutdown.borrow_and_update()).unwrap_or_else(fatal_drain_deadline);
        match self.drain_to(deadline).await {
            Some(message) => Err(anyhow::anyhow!(
                "local IPC session task failed while shutting down: {message}"
            )),
            None => Ok(()),
        }
    }

    async fn drain_to(&mut self, deadline: Instant) -> Option<String> {
        let failure = drain_sessions(&mut self.sessions, &mut self.shutdown, deadline).await;
        #[cfg(test)]
        if let Some(drained) = self.sessions_drained.take() {
            debug_assert!(self.sessions.is_empty());
            let _ = drained.send(());
        }
        failure
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
    service: SearchService,
) -> anyhow::Result<EndpointCleanupV1> {
    IpcService::new(service).serve(endpoint).await
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
) -> Option<String> {
    let mut observe_tightening = true;
    let mut first_failure = None;
    loop {
        if sessions.is_empty() {
            return first_failure;
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
                if let Some(Err(error)) = joined
                    && first_failure.is_none()
                {
                    first_failure = Some(crate::truncate_utf8(error.to_string(), 4 * 1024));
                }
            }
            () = tokio::time::sleep_until(deadline) => break,
        }
    }
    sessions.abort_all();
    while let Some(result) = sessions.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
            && first_failure.is_none()
        {
            first_failure = Some(crate::truncate_utf8(error.to_string(), 4 * 1024));
        }
    }
    first_failure
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
) -> Result<(VerifiedFramedTransportV1, ConnectionLease), EndpointTransportError> {
    let lease = connections.acquire_connection().await;
    // The platform listener remains armed while capacity is unavailable. The Windows transport
    // retains exactly one single-use pending slot until ownership transfers into this future.
    let stream = endpoint.accept_verified().await?;
    Ok((stream, lease))
}

async fn session(
    mut stream: VerifiedFramedTransportV1,
    service: SearchService,
    connections: ConnectionCapacity,
    connection: ConnectionLease,
    project_id: unity_asset_search_protocol::ProjectId,
    daemon_instance_id: unity_asset_search_protocol::DaemonInstanceId,
) -> Result<(), SessionError> {
    let preclassification_deadline = SessionDeadline::new()?;
    let Some(bootstrap_result) = preclassification_deadline
        .run(stream.read_frame(
            FrameLimits::bootstrap(),
            FrameReadTimeoutsV1::new(BOOTSTRAP_TIMEOUT, BODY_TIMEOUT),
        ))
        .await
    else {
        return Ok(());
    };
    let bootstrap_frame = bootstrap_result?.ok_or(SessionError::ClosedDuringBootstrap)?;
    let mut budget = AssetLoadBudget::default();
    let hello: BootstrapHelloV2 =
        decode_validated_frame(&bootstrap_frame, &mut budget, FrameLimits::bootstrap())?;
    if preclassification_deadline.expired() {
        return Ok(());
    }
    let reply = BootstrapReplyV2::negotiate(
        &hello,
        project_id,
        daemon_instance_id,
        service.query_policy_id(),
        &[BUSINESS_PROTOCOL_REVISION],
    );
    let reply_frame = encode_frame(&reply, FrameLimits::bootstrap())?;
    if preclassification_deadline.expired() {
        return Ok(());
    }
    let Some(reply_result) = preclassification_deadline
        .run(stream.write_frame_monitoring_inbound(
            &reply_frame,
            FrameLimits::bootstrap(),
            WRITE_TIMEOUT,
            FramedPeerStateV1::Open,
        ))
        .await
    else {
        return Ok(());
    };
    match reply_result? {
        FramedPeerStateV1::Open => {}
        FramedPeerStateV1::Closed => return Ok(()),
        FramedPeerStateV1::Pipelined => return Err(SessionError::PipelinedRequest),
    }
    if reply.selected_revision().is_none() {
        return Ok(());
    }

    let Some(first_frame_result) = preclassification_deadline
        .run(stream.read_frame(
            FrameLimits::request_envelope(),
            FrameReadTimeoutsV1::new(IDLE_TIMEOUT, BODY_TIMEOUT),
        ))
        .await
    else {
        return Ok(());
    };
    let Some(first_frame) = first_frame_result? else {
        return Ok(());
    };
    let mut budget = AssetLoadBudget::default();
    let first_request = decode_request_frame(&first_frame, &mut budget)?;
    first_request.validate_binding(project_id, daemon_instance_id, service.query_policy_id())?;
    if preclassification_deadline.expired() {
        return Ok(());
    }
    let first_class = RequestClass::for_operation(first_request.operation().kind());
    let classified = match connections
        .admit_classified(first_class, preclassification_deadline)
        .await
    {
        Some(classified) => classified,
        None => {
            let maximum = match first_class {
                RequestClass::Control => ORDINARY_CONNECTIONS + CONTROL_RESERVED_CONNECTIONS,
                RequestClass::Work | RequestClass::Wait => ORDINARY_CONNECTIONS,
            };
            let saturated = SearchServiceResult {
                response: Err(ApiError::new(
                    ApiErrorCode::Busy,
                    "daemon persistent IPC session capacity reached",
                    true,
                )
                .with_detail("class", first_class.name())
                .with_detail("maximum", maximum.to_string())
                .with_query_policy(service.query_policy_id())),
                shutdown_after_response: None,
            };
            let _connection = connection;
            let _ = write_dispatch_response(
                &mut stream,
                &service,
                &first_request,
                saturated,
                FramedPeerStateV1::Open,
                Some(preclassification_deadline),
            )
            .await?;
            return Ok(());
        }
    };
    let capacity_lane = classified.capacity_lane();
    let reserved_session_deadline = capacity_lane.absolute_deadline(preclassification_deadline);
    let _connection = connection;
    let _classified = classified;
    let first_deadline =
        (first_class == RequestClass::Control).then_some(preclassification_deadline);
    if handle_request(&mut stream, &service, first_request, first_deadline).await?
        == RequestDisposition::Close
    {
        return Ok(());
    }

    loop {
        let Some(frame_result) = run_before(
            reserved_session_deadline,
            stream.read_frame(
                FrameLimits::request_envelope(),
                FrameReadTimeoutsV1::new(IDLE_TIMEOUT, BODY_TIMEOUT),
            ),
        )
        .await
        else {
            return Ok(());
        };
        let frame = frame_result?;
        let Some(frame) = frame else { return Ok(()) };
        let frame_processing_deadline = SessionDeadline::new()?;
        let mut budget = AssetLoadBudget::default();
        let request = decode_request_frame(&frame, &mut budget)?;
        request.validate_binding(project_id, daemon_instance_id, service.query_policy_id())?;
        if frame_processing_deadline.expired() {
            return Ok(());
        }
        let class = RequestClass::for_operation(request.operation().kind());
        if !capacity_lane.permits(class) {
            let rejected = SearchServiceResult {
                response: Err(ApiError::new(
                    ApiErrorCode::Busy,
                    "control-reserved connection only accepts control operations",
                    true,
                )
                .with_detail("lane", "control_reserved")
                .with_detail("accepted_class", RequestClass::Control.name())
                .with_query_policy(service.query_policy_id())),
                shutdown_after_response: None,
            };
            let _ = write_dispatch_response(
                &mut stream,
                &service,
                &request,
                rejected,
                FramedPeerStateV1::Open,
                reserved_session_deadline,
            )
            .await?;
            return Ok(());
        }
        let deadline = if let Some(deadline) = reserved_session_deadline {
            Some(deadline)
        } else if class == RequestClass::Control {
            Some(frame_processing_deadline)
        } else {
            None
        };
        if handle_request(&mut stream, &service, request, deadline).await?
            == RequestDisposition::Close
        {
            return Ok(());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestDisposition {
    Continue,
    Close,
}

async fn handle_request(
    stream: &mut VerifiedFramedTransportV1,
    service: &SearchService,
    request: RequestEnvelope,
    deadline: Option<SessionDeadline>,
) -> Result<RequestDisposition, SessionError> {
    let operation = request.operation().clone();
    let request_service = service.clone();
    let dispatch = async move { request_service.execute(operation).await };
    let Some((dispatched, peer_state)) =
        run_before(deadline, stream.monitor_inbound_while(dispatch)).await
    else {
        return Ok(RequestDisposition::Close);
    };
    if peer_state == FramedPeerStateV1::Closed {
        if let Some(shutdown_deadline) = dispatched.shutdown_after_response {
            service.begin_shutdown_at(shutdown_deadline);
        }
        return Ok(RequestDisposition::Close);
    }
    write_dispatch_response(stream, service, &request, dispatched, peer_state, deadline).await
}

async fn write_dispatch_response(
    stream: &mut VerifiedFramedTransportV1,
    service: &SearchService,
    request: &RequestEnvelope,
    dispatched: SearchServiceResult,
    peer_state: FramedPeerStateV1,
    deadline: Option<SessionDeadline>,
) -> Result<RequestDisposition, SessionError> {
    let response_limits = FrameLimits::response(request.operation().kind());
    let shutdown_after_response = dispatched.shutdown_after_response;
    let response = match dispatched.response {
        Ok(response) => ResponseEnvelope::success(request, response),
        Err(error) => ResponseEnvelope::error(request, error),
    };
    let Some(response_frame) =
        run_synchronous_before(deadline, || encode_response_frame(&response, request))?
    else {
        if let Some(shutdown_deadline) = shutdown_after_response {
            service.begin_shutdown_at(shutdown_deadline);
        }
        return Ok(RequestDisposition::Close);
    };
    let write = async {
        stream
            .write_frame_monitoring_inbound(
                &response_frame,
                response_limits,
                WRITE_TIMEOUT,
                peer_state,
            )
            .await
            .map_err(SessionError::from)
    };
    let write_result = run_before(deadline, write).await;
    if let Some(shutdown_deadline) = shutdown_after_response {
        service.begin_shutdown_at(shutdown_deadline);
    }
    let Some(write_result) = write_result else {
        return Ok(RequestDisposition::Close);
    };
    let peer_state = write_result?;
    if peer_state == FramedPeerStateV1::Pipelined {
        return Err(SessionError::PipelinedRequest);
    }
    if shutdown_after_response.is_some() || peer_state == FramedPeerStateV1::Closed {
        return Ok(RequestDisposition::Close);
    }
    Ok(RequestDisposition::Continue)
}

fn run_synchronous_before<T, E>(
    deadline: Option<SessionDeadline>,
    operation: impl FnOnce() -> Result<T, E>,
) -> Result<Option<T>, E> {
    let output = operation()?;
    Ok(deadline
        .is_none_or(|deadline| !deadline.expired())
        .then_some(output))
}

async fn run_before<F, T>(deadline: Option<SessionDeadline>, future: F) -> Option<T>
where
    F: Future<Output = T>,
{
    match deadline {
        Some(deadline) => deadline.run(future).await,
        None => Some(future.await),
    }
}

#[derive(Debug, thiserror::Error)]
enum SessionError {
    #[error("peer closed during bootstrap")]
    ClosedDuringBootstrap,
    #[error("client pipelined a second request")]
    PipelinedRequest,
    #[error(transparent)]
    Transport(#[from] EndpointTransportError),
    #[error(transparent)]
    Framing(#[from] unity_asset_search_protocol::FramingError),
    #[error(transparent)]
    Contract(#[from] unity_asset_search_protocol::ContractValidationError),
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant as StdInstant},
    };

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::sync::watch;
    use tokio::time::Instant;
    use unity_asset_core::AssetLoadBudget;
    use unity_asset_search_index::{IndexPaths, SearchIndex, SearchIndexOptions};
    use unity_asset_search_local::{
        EndpointCleanupV1, FrameReadTimeoutsV1, PrivateRootsV1, VerifiedFramedTransportV1,
        generate_daemon_instance_id,
    };
    use unity_asset_search_protocol::{
        ApiErrorCode, BUSINESS_PROTOCOL_REVISION, BootstrapHelloV2, BootstrapReplyV2,
        CapabilitiesRequest, DaemonInstanceId, FilesystemReindexIntent, FrameLimits,
        MAX_WAIT_TIMEOUT_MS, ProjectId, ReindexAdmitRequest, ReindexCancelRequest,
        ReindexStatusRequest, ReindexWaitRequest, RequestEnvelope, RequestId, RequestOperation,
        ResponseEnvelope, ResponseOperation, ResponseOutcome, SearchRequest, ShutdownRequest,
        StatusRequest, decode_response_frame, decode_validated_frame, encode_frame,
        encode_request_frame,
    };

    use super::{
        CONTROL_ADMISSION_RESPONSE_RESERVE, CONTROL_RESERVED_CONNECTIONS, ConnectionCapacity,
        MAX_LOCAL_IPC_CONNECTIONS_V1, ORDINARY_CONNECTIONS, PRECLASSIFICATION_AND_CONTROL_TIMEOUT,
        ServeEvent, SessionCapacityLane, SessionDeadline, SessionError,
        UNCLASSIFIED_CONNECTION_HEADROOM, drain_sessions, next_serve_event, run_synchronous_before,
        session,
    };
    use crate::coordinator::{ReindexCoordinatorConfig, ReindexCoordinatorRuntime};
    use crate::lifecycle::{AdmissionGate, BlockingTaskOwner};
    use crate::operations::OperationServiceOwner;
    use crate::service::{RequestClass, SearchService};
    use crate::watcher::MaintenanceRuntime;

    #[tokio::test]
    async fn control_reserved_capacity_is_unavailable_until_request_classification() {
        assert_eq!(
            UNCLASSIFIED_CONNECTION_HEADROOM + ORDINARY_CONNECTIONS + CONTROL_RESERVED_CONNECTIONS,
            MAX_LOCAL_IPC_CONNECTIONS_V1
        );
        let capacity = ConnectionCapacity::new(3, 1, 1);
        let connection = capacity.acquire_connection().await;
        assert_eq!(capacity.control_reserved.available_permits(), 1);

        let ordinary = capacity
            .admit_classified(RequestClass::Work, SessionDeadline::new().unwrap())
            .await
            .unwrap();
        assert_eq!(ordinary.capacity_lane(), SessionCapacityLane::Ordinary);
        assert_eq!(capacity.control_reserved.available_permits(), 1);
        assert!(
            capacity
                .admit_classified(RequestClass::Work, SessionDeadline::new().unwrap())
                .await
                .is_none()
        );
        assert_eq!(capacity.control_reserved.available_permits(), 1);

        let reserved = capacity
            .admit_classified(RequestClass::Control, SessionDeadline::new().unwrap())
            .await
            .unwrap();
        assert_eq!(
            reserved.capacity_lane(),
            SessionCapacityLane::ControlReserved
        );
        assert_eq!(capacity.control_reserved.available_permits(), 0);

        drop(reserved);
        drop(connection);
        drop(ordinary);
    }

    #[tokio::test]
    async fn production_connection_capacity_reserves_the_declared_control_lane() {
        let capacity = ConnectionCapacity::production();
        let mut connections = Vec::with_capacity(MAX_LOCAL_IPC_CONNECTIONS_V1);
        let mut ordinary = Vec::with_capacity(ORDINARY_CONNECTIONS);
        let mut control_reserved = Vec::with_capacity(CONTROL_RESERVED_CONNECTIONS);

        for _ in 0..ORDINARY_CONNECTIONS {
            connections.push(capacity.acquire_connection().await);
            let lease = capacity
                .admit_classified(RequestClass::Work, SessionDeadline::new().unwrap())
                .await
                .unwrap();
            assert_eq!(lease.capacity_lane(), SessionCapacityLane::Ordinary);
            ordinary.push(lease);
        }
        for _ in 0..CONTROL_RESERVED_CONNECTIONS {
            connections.push(capacity.acquire_connection().await);
            let lease = capacity
                .admit_classified(RequestClass::Control, SessionDeadline::new().unwrap())
                .await
                .unwrap();
            assert_eq!(lease.capacity_lane(), SessionCapacityLane::ControlReserved);
            control_reserved.push(lease);
        }
        assert_eq!(
            capacity.total.available_permits(),
            UNCLASSIFIED_CONNECTION_HEADROOM
        );
        for _ in 0..UNCLASSIFIED_CONNECTION_HEADROOM {
            connections.push(capacity.acquire_connection().await);
        }

        assert_eq!(connections.len(), MAX_LOCAL_IPC_CONNECTIONS_V1);
        assert_eq!(capacity.total.available_permits(), 0);
        assert_eq!(capacity.ordinary.available_permits(), 0);
        assert_eq!(capacity.control_reserved.available_permits(), 0);

        drop(control_reserved.pop().unwrap());
        let reclaimed_control = capacity
            .admit_classified(RequestClass::Control, SessionDeadline::new().unwrap())
            .await
            .unwrap();
        assert_eq!(
            reclaimed_control.capacity_lane(),
            SessionCapacityLane::ControlReserved
        );
        drop(reclaimed_control);

        drop(ordinary.pop().unwrap());
        let reclaimed_ordinary = capacity
            .admit_classified(RequestClass::Work, SessionDeadline::new().unwrap())
            .await
            .unwrap();
        assert_eq!(
            reclaimed_ordinary.capacity_lane(),
            SessionCapacityLane::Ordinary
        );
        drop(connections);
    }

    #[tokio::test(start_paused = true)]
    async fn spare_global_capacity_preclassifies_two_headroom_batches_together() {
        const CLI_DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
        const BOOTSTRAP_ELAPSED: Duration = Duration::from_secs(3);
        const STALLED_CONNECTIONS: usize =
            UNCLASSIFIED_CONNECTION_HEADROOM + CONTROL_RESERVED_CONNECTIONS;

        assert!(PRECLASSIFICATION_AND_CONTROL_TIMEOUT < CLI_DEFAULT_CONNECT_TIMEOUT);
        let capacity = ConnectionCapacity::production();
        let mut ordinary_connections = Vec::with_capacity(ORDINARY_CONNECTIONS);
        let mut ordinary = Vec::with_capacity(ORDINARY_CONNECTIONS);
        for _ in 0..ORDINARY_CONNECTIONS {
            ordinary_connections.push(capacity.acquire_connection().await);
            let lease = capacity
                .admit_classified(RequestClass::Work, SessionDeadline::new().unwrap())
                .await
                .unwrap();
            ordinary.push(lease);
        }

        let started = Instant::now();
        let mut stalled = tokio::task::JoinSet::new();
        for _ in 0..STALLED_CONNECTIONS {
            let lease = capacity.acquire_connection().await;
            let deadline = SessionDeadline::new().unwrap();
            let (mut client, mut server) = tokio::io::duplex(8);
            stalled.spawn(async move {
                let _lease = lease;
                deadline
                    .run(async {
                        tokio::join!(
                            async {
                                tokio::time::sleep(BOOTSTRAP_ELAPSED).await;
                                client.write_all(&[0, 0, 0, 1, 0]).await.unwrap();
                                std::future::pending::<()>().await;
                            },
                            async {
                                let mut bootstrap = [0_u8; 6];
                                server.read_exact(&mut bootstrap).await.unwrap();
                            }
                        );
                    })
                    .await
            });
        }
        assert_eq!(capacity.ordinary.available_permits(), 0);
        assert_eq!(capacity.total.available_permits(), 0);
        assert_eq!(
            capacity.control_reserved.available_permits(),
            CONTROL_RESERVED_CONNECTIONS
        );
        tokio::task::yield_now().await;

        let replenished =
            tokio::time::timeout(CLI_DEFAULT_CONNECT_TIMEOUT, capacity.acquire_connection())
                .await
                .expect("partial-frame capacity must recycle before the CLI connect deadline");
        assert!(Instant::now().duration_since(started) <= PRECLASSIFICATION_AND_CONTROL_TIMEOUT);
        assert_eq!(
            capacity.control_reserved.available_permits(),
            CONTROL_RESERVED_CONNECTIONS
        );
        let status = capacity
            .admit_classified(RequestClass::Control, SessionDeadline::new().unwrap())
            .await
            .expect("a classified status request retains reserved admission");
        assert_eq!(status.capacity_lane(), SessionCapacityLane::ControlReserved);
        drop(status);
        drop(replenished);

        while let Some(result) = stalled.join_next().await {
            assert!(result.unwrap().is_none());
        }
        drop(ordinary_connections);
        drop(ordinary);
    }

    async fn assert_classified_control_uses_lane_reopened_first(expected: SessionCapacityLane) {
        let capacity = ConnectionCapacity::new(3, 1, 1);
        let ordinary = capacity
            .admit_classified(RequestClass::Work, SessionDeadline::new().unwrap())
            .await
            .unwrap();
        let reserved = capacity
            .admit_classified(RequestClass::Control, SessionDeadline::new().unwrap())
            .await
            .unwrap();
        assert_eq!(
            reserved.capacity_lane(),
            SessionCapacityLane::ControlReserved
        );

        let waiting_capacity = capacity.clone();
        let waiting = tokio::spawn(async move {
            waiting_capacity
                .admit_classified(RequestClass::Control, SessionDeadline::new().unwrap())
                .await
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        let still_held = match expected {
            SessionCapacityLane::Ordinary => {
                drop(ordinary);
                reserved
            }
            SessionCapacityLane::ControlReserved => {
                drop(reserved);
                ordinary
            }
        };
        let admitted = waiting.await.unwrap().unwrap();
        assert_eq!(admitted.capacity_lane(), expected);
        drop(still_held);
    }

    #[tokio::test(start_paused = true)]
    async fn classified_control_uses_an_ordinary_lane_that_reopens_first() {
        assert_classified_control_uses_lane_reopened_first(SessionCapacityLane::Ordinary).await;
    }

    #[tokio::test(start_paused = true)]
    async fn classified_control_uses_a_reserved_lane_that_reopens_first() {
        assert_classified_control_uses_lane_reopened_first(SessionCapacityLane::ControlReserved)
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn saturated_control_admission_leaves_time_for_a_structured_busy_response() {
        let capacity = ConnectionCapacity::new(3, 1, 1);
        let ordinary = capacity
            .admit_classified(RequestClass::Work, SessionDeadline::new().unwrap())
            .await
            .unwrap();
        let reserved = capacity
            .admit_classified(RequestClass::Control, SessionDeadline::new().unwrap())
            .await
            .unwrap();
        let deadline = SessionDeadline::new().unwrap();
        let started = Instant::now();

        assert!(
            capacity
                .admit_classified(RequestClass::Control, deadline)
                .await
                .is_none()
        );
        assert_eq!(
            Instant::now().duration_since(started),
            PRECLASSIFICATION_AND_CONTROL_TIMEOUT - CONTROL_ADMISSION_RESPONSE_RESERVE
        );
        assert!(deadline.run(std::future::ready(())).await.is_some());

        drop(reserved);
        drop(ordinary);
    }

    #[tokio::test(start_paused = true)]
    async fn non_reading_control_response_cannot_reset_the_absolute_deadline() {
        const DISPATCH_ELAPSED: Duration = Duration::from_secs(3);

        let deadline = SessionDeadline::new().unwrap();
        let started = Instant::now();
        assert!(
            deadline
                .run(tokio::time::sleep(DISPATCH_ELAPSED))
                .await
                .is_some()
        );
        let (mut server, _non_reading_client) = tokio::io::duplex(1);

        let write = deadline.run(server.write_all(&[0_u8; 8])).await;

        assert!(write.is_none());
        assert_eq!(
            Instant::now().duration_since(started),
            PRECLASSIFICATION_AND_CONTROL_TIMEOUT
        );
    }

    #[test]
    fn synchronous_response_work_finishing_after_the_deadline_is_discarded() {
        let deadline = SessionDeadline {
            at: Instant::now() + Duration::from_millis(10),
        };
        let mut encoded = false;

        let response = run_synchronous_before(Some(deadline), || {
            std::thread::sleep(Duration::from_millis(20));
            encoded = true;
            Ok::<_, SessionError>([1_u8])
        })
        .unwrap();

        assert!(encoded);
        assert!(response.is_none());
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

        assert_eq!(
            drain_sessions(&mut sessions, &mut receiver, Instant::now()).await,
            None
        );

        assert!(sessions.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_shutdown_drain_aborts_after_requested_timeout() {
        let mut sessions = tokio::task::JoinSet::new();
        sessions.spawn(std::future::pending::<()>());
        let (_shutdown, mut receiver) = watch::channel(None);

        assert_eq!(
            drain_sessions(
                &mut sessions,
                &mut receiver,
                Instant::now() + Duration::from_millis(5),
            )
            .await,
            None
        );

        assert!(sessions.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn active_shutdown_drain_observes_a_tighter_deadline() {
        let mut sessions = tokio::task::JoinSet::new();
        sessions.spawn(std::future::pending::<()>());
        let (shutdown, mut receiver) = watch::channel(None);
        let initial = Instant::now() + Duration::from_secs(60);
        let drain = tokio::spawn(async move {
            let failure = drain_sessions(&mut sessions, &mut receiver, initial).await;
            (sessions, failure)
        });
        tokio::task::yield_now().await;

        shutdown.send(Some(Instant::now())).unwrap();
        let (sessions, failure) = drain.await.unwrap();

        assert!(sessions.is_empty());
        assert_eq!(failure, None);
        assert!(Instant::now() < initial);
    }

    #[tokio::test]
    async fn session_join_failure_is_returned_as_lifecycle_evidence() {
        let mut sessions = tokio::task::JoinSet::new();
        sessions.spawn(async move {
            panic!("session fixture panic");
        });
        let (_shutdown, mut receiver) = watch::channel(None);

        let failure = drain_sessions(
            &mut sessions,
            &mut receiver,
            Instant::now() + Duration::from_secs(1),
        )
        .await;

        assert!(failure.is_some_and(|message| message.contains("session fixture panic")));
        assert!(sessions.is_empty());
    }

    #[derive(Debug, Clone, Copy)]
    enum SessionCase {
        Bootstrap,
        PipelinedBusiness,
        SequentialBusiness,
        OrdinaryControlThenWork,
        SequentialReservedControl,
        ReservedControlThenWork,
        SaturatedWork,
    }

    async fn bootstrap_client(
        client: &mut VerifiedFramedTransportV1,
        project_id: ProjectId,
        instance_id: DaemonInstanceId,
    ) {
        let hello =
            BootstrapHelloV2::new(project_id, instance_id, vec![BUSINESS_PROTOCOL_REVISION])
                .unwrap();
        client
            .write_frame(
                &encode_frame(&hello, FrameLimits::bootstrap()).unwrap(),
                FrameLimits::bootstrap(),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        let reply_frame = client
            .read_frame(
                FrameLimits::bootstrap(),
                FrameReadTimeoutsV1::uniform(Duration::from_secs(5)),
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
        client: &mut VerifiedFramedTransportV1,
        request: &RequestEnvelope,
    ) -> ResponseEnvelope {
        let request_frame = encode_request_frame(request).unwrap();
        client
            .write_frame(
                &request_frame,
                FrameLimits::request_envelope(),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        let response_frame = client
            .read_frame(
                FrameLimits::response(request.operation().kind()),
                FrameReadTimeoutsV1::uniform(Duration::from_secs(5)),
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
            ReindexCoordinatorConfig::new(index.paths().project_path_space().clone())
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
        let operation_service = OperationServiceOwner::new(
            instance_id,
            coordinator.clone(),
            lifecycle_admission.clone(),
        );
        let maintenance = MaintenanceRuntime::start(operation_service.service(), None, None);
        let service = SearchService::new(
            index,
            _blocking_tasks.handle(),
            operation_service.service(),
            query_policy_id,
            lifecycle_admission,
            maintenance.handle(),
        );
        let ordinary_connections = usize::from(!matches!(
            case,
            SessionCase::SequentialReservedControl
                | SessionCase::ReservedControlThenWork
                | SessionCase::SaturatedWork
        ));
        let control_reserved_connections =
            usize::from(!matches!(case, SessionCase::OrdinaryControlThenWork));
        let connections =
            ConnectionCapacity::new(1, ordinary_connections, control_reserved_connections);
        let connection = connections.acquire_connection().await;

        let hello =
            BootstrapHelloV2::new(project_id, instance_id, vec![BUSINESS_PROTOCOL_REVISION])
                .unwrap();
        let hello_frame = encode_frame(&hello, FrameLimits::bootstrap()).unwrap();
        let first_operation = if matches!(
            case,
            SessionCase::OrdinaryControlThenWork
                | SessionCase::SequentialReservedControl
                | SessionCase::ReservedControlThenWork
        ) {
            RequestOperation::Capabilities(CapabilitiesRequest::default())
        } else {
            RequestOperation::ReindexAdmit(ReindexAdmitRequest {
                intent: FilesystemReindexIntent::full(),
                idempotency_key: None,
            })
        };
        let first_request = RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([7; 16]),
            project_id,
            instance_id,
            service.query_policy_id(),
            first_operation,
        )
        .unwrap();
        let first_frame = encode_request_frame(&first_request).unwrap();

        connected
            .write_frame(
                &hello_frame,
                FrameLimits::bootstrap(),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        if matches!(case, SessionCase::Bootstrap) {
            connected
                .write_frame(
                    &first_frame,
                    FrameLimits::request_envelope(),
                    Duration::from_secs(5),
                )
                .await
                .unwrap();
        }
        let server_session = tokio::spawn(session(
            accepted,
            service.clone(),
            connections,
            connection,
            project_id,
            instance_id,
        ));

        let reply_frame = connected
            .read_frame(
                FrameLimits::bootstrap(),
                FrameReadTimeoutsV1::uniform(Duration::from_secs(5)),
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
            let second_operation = if matches!(case, SessionCase::SequentialReservedControl) {
                RequestOperation::Status(StatusRequest::default())
            } else {
                RequestOperation::ReindexAdmit(ReindexAdmitRequest {
                    intent: FilesystemReindexIntent::reconcile(),
                    idempotency_key: None,
                })
            };
            let second_request = RequestEnvelope::new(
                BUSINESS_PROTOCOL_REVISION,
                RequestId::from_bytes([8; 16]),
                project_id,
                instance_id,
                service.query_policy_id(),
                second_operation,
            )
            .unwrap();
            let second_frame = encode_request_frame(&second_request).unwrap();
            if matches!(case, SessionCase::PipelinedBusiness) {
                connected
                    .write_frame(
                        &first_frame,
                        FrameLimits::request_envelope(),
                        Duration::from_secs(5),
                    )
                    .await
                    .unwrap();
                connected
                    .write_frame(
                        &second_frame,
                        FrameLimits::request_envelope(),
                        Duration::from_secs(5),
                    )
                    .await
                    .unwrap();
                assert!(
                    connected
                        .read_frame(
                            FrameLimits::response(first_request.operation().kind()),
                            FrameReadTimeoutsV1::uniform(Duration::from_secs(5)),
                        )
                        .await
                        .unwrap()
                        .is_some()
                );
            } else {
                connected
                    .write_frame(
                        &first_frame,
                        FrameLimits::request_envelope(),
                        Duration::from_secs(5),
                    )
                    .await
                    .unwrap();
                let response_frame = connected
                    .read_frame(
                        FrameLimits::response(first_request.operation().kind()),
                        FrameReadTimeoutsV1::uniform(Duration::from_secs(5)),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                if matches!(case, SessionCase::SaturatedWork) {
                    let mut budget = AssetLoadBudget::default();
                    let response =
                        decode_response_frame(&response_frame, &mut budget, &first_request)
                            .unwrap();
                    let ResponseOutcome::Error(error) = response.into_outcome() else {
                        panic!("reserved work request must return a structured error");
                    };
                    assert_eq!(error.code, ApiErrorCode::Busy);
                    assert_eq!(error.details.get("class").map(String::as_str), Some("work"));
                } else {
                    if matches!(
                        case,
                        SessionCase::OrdinaryControlThenWork
                            | SessionCase::SequentialReservedControl
                            | SessionCase::ReservedControlThenWork
                    ) {
                        let mut budget = AssetLoadBudget::default();
                        let response =
                            decode_response_frame(&response_frame, &mut budget, &first_request)
                                .unwrap();
                        let ResponseOutcome::Success(response) = response.into_outcome() else {
                            panic!("control-first request returned an error");
                        };
                        assert!(matches!(*response, ResponseOperation::Capabilities(_)));
                    }
                    connected
                        .write_frame(
                            &second_frame,
                            FrameLimits::request_envelope(),
                            Duration::from_secs(5),
                        )
                        .await
                        .unwrap();
                    let second_response = connected
                        .read_frame(
                            FrameLimits::response(second_request.operation().kind()),
                            FrameReadTimeoutsV1::uniform(Duration::from_secs(5)),
                        )
                        .await
                        .unwrap()
                        .unwrap();
                    if matches!(case, SessionCase::SequentialReservedControl) {
                        let mut budget = AssetLoadBudget::default();
                        let response =
                            decode_response_frame(&second_response, &mut budget, &second_request)
                                .unwrap();
                        let ResponseOutcome::Success(response) = response.into_outcome() else {
                            panic!("following status request returned an error");
                        };
                        assert!(matches!(*response, ResponseOperation::Status(_)));
                    } else if matches!(case, SessionCase::OrdinaryControlThenWork) {
                        let mut budget = AssetLoadBudget::default();
                        let response =
                            decode_response_frame(&second_response, &mut budget, &second_request)
                                .unwrap();
                        let ResponseOutcome::Success(response) = response.into_outcome() else {
                            panic!("ordinary control-first session rejected following work");
                        };
                        assert!(matches!(*response, ResponseOperation::ReindexAdmit(_)));
                    } else if matches!(case, SessionCase::ReservedControlThenWork) {
                        let mut budget = AssetLoadBudget::default();
                        let response =
                            decode_response_frame(&second_response, &mut budget, &second_request)
                                .unwrap();
                        let ResponseOutcome::Error(error) = response.into_outcome() else {
                            panic!("reserved session admitted a following work request");
                        };
                        assert_eq!(error.code, ApiErrorCode::Busy);
                        assert_eq!(
                            error.details.get("lane").map(String::as_str),
                            Some("control_reserved")
                        );
                    }
                }
            }
        }

        drop(connected);
        let session_result = server_session.await.unwrap();
        let admissions = coordinator.snapshot().await.admissions.client;
        if matches!(
            case,
            SessionCase::SequentialBusiness
                | SessionCase::OrdinaryControlThenWork
                | SessionCase::SequentialReservedControl
                | SessionCase::ReservedControlThenWork
                | SessionCase::SaturatedWork
        ) {
            assert!(session_result.is_ok());
        } else {
            assert!(
                matches!(session_result, Err(SessionError::PipelinedRequest)),
                "unexpected session result for {case:?}: {session_result:?}; admissions={admissions}"
            );
        }

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
    async fn control_first_ordinary_session_reuses_connection_for_following_work() {
        assert_eq!(
            run_session_case(SessionCase::OrdinaryControlThenWork).await,
            1
        );
    }

    #[tokio::test]
    async fn control_reserved_session_reuses_connection_for_following_status() {
        assert_eq!(
            run_session_case(SessionCase::SequentialReservedControl).await,
            0
        );
    }

    #[tokio::test]
    async fn control_reserved_session_rejects_following_work_before_dispatch() {
        assert_eq!(
            run_session_case(SessionCase::ReservedControlThenWork).await,
            0
        );
    }

    #[tokio::test]
    async fn saturated_ordinary_capacity_rejects_work_before_dispatch() {
        assert_eq!(run_session_case(SessionCase::SaturatedWork).await, 0);
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
        let executor_release = Arc::new(tokio::sync::Notify::new());
        let executor_release_task = Arc::clone(&executor_release);
        let mut coordinator_runtime = ReindexCoordinatorRuntime::start(
            ReindexCoordinatorConfig::new(index.paths().project_path_space().clone())
                .with_debounce(Duration::from_millis(10))
                .with_max_debounce(Duration::from_millis(10)),
            move |_intent| {
                let executor_release = Arc::clone(&executor_release_task);
                async move {
                    executor_release.notified().await;
                    Err(anyhow::anyhow!("test executor released"))
                }
            },
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
        let mut operation_service = OperationServiceOwner::new(
            instance_id,
            coordinator.clone(),
            lifecycle_admission.clone(),
        );
        let mut maintenance = MaintenanceRuntime::start(operation_service.service(), None, None);
        let service = SearchService::new(
            index,
            blocking_tasks.handle(),
            operation_service.service(),
            query_policy_id,
            lifecycle_admission,
            maintenance.handle(),
        );
        let server = tokio::spawn(async move {
            let mut endpoint = endpoint;
            let result = super::serve(&mut endpoint, service).await;
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

        let admission = RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([20; 16]),
            project_id,
            instance_id,
            query_policy_id,
            RequestOperation::ReindexAdmit(ReindexAdmitRequest {
                intent: FilesystemReindexIntent::full(),
                idempotency_key: None,
            }),
        )
        .unwrap();
        let ResponseOutcome::Success(response) = exchange_request(&mut work_client, &admission)
            .await
            .into_outcome()
        else {
            panic!("daemon rejected the operation used to prove draining wait admission");
        };
        let ResponseOperation::ReindexAdmit(operation) = response.as_ref() else {
            panic!("reindex admission returned the wrong response operation");
        };
        assert!(!operation.state.is_terminal());
        let active_operation = operation.operation_id;
        let admissions_before = coordinator.snapshot().await.admissions.client;

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

        let operation_status = RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([23; 16]),
            project_id,
            instance_id,
            query_policy_id,
            RequestOperation::ReindexStatus(ReindexStatusRequest {
                operation_id: active_operation,
            }),
        )
        .unwrap();
        let ResponseOutcome::Success(response) =
            exchange_request(&mut work_client, &operation_status)
                .await
                .into_outcome()
        else {
            panic!("draining daemon hid an existing operation status");
        };
        assert!(matches!(
            response.as_ref(),
            ResponseOperation::ReindexStatus(operation)
                if operation.operation_id == active_operation && !operation.state.is_terminal()
        ));

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

        let wait = RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([26; 16]),
            project_id,
            instance_id,
            query_policy_id,
            RequestOperation::ReindexWait(ReindexWaitRequest {
                operation_id: active_operation,
                timeout_ms: MAX_WAIT_TIMEOUT_MS,
            }),
        )
        .unwrap();
        let wait_response = tokio::time::timeout(
            Duration::from_secs(1),
            exchange_request(&mut work_client, &wait),
        )
        .await
        .expect("draining admission must reject a long operation wait immediately");
        let ResponseOutcome::Error(error) = wait_response.into_outcome() else {
            panic!("draining daemon admitted a new operation wait");
        };
        assert_eq!(error.code, ApiErrorCode::NotReady);

        let cancel = RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([27; 16]),
            project_id,
            instance_id,
            query_policy_id,
            RequestOperation::ReindexCancel(ReindexCancelRequest {
                operation_id: active_operation,
            }),
        )
        .unwrap();
        let ResponseOutcome::Error(error) = exchange_request(&mut work_client, &cancel)
            .await
            .into_outcome()
        else {
            panic!("draining daemon admitted operation state mutation");
        };
        assert_eq!(error.code, ApiErrorCode::NotReady);
        assert_eq!(
            coordinator.snapshot().await.admissions.client,
            admissions_before
        );

        drop(shutdown_client);
        drop(work_client);
        let (serve_result, endpoint) = server.await.unwrap();
        assert_eq!(serve_result.unwrap(), EndpointCleanupV1::Removed);
        maintenance.shutdown().await.unwrap();
        executor_release.notify_one();
        coordinator_runtime.shutdown().await.unwrap();
        operation_service.shutdown().await.unwrap();
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
