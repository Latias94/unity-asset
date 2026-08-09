use std::fmt;
use std::future::Future;
use std::io;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use unity_asset_search_protocol::{DaemonInstanceId, FrameLimits};

use crate::endpoint_store::DaemonLeaseV1;
use crate::{
    DiscoveredEndpointV1, EndpointDescriptorError, EndpointNamespaceV1, EndpointStoreError,
    ProcessIdentityError, ProcessIdentityV1, SecurityContextError, SecurityContextIdV1,
};

/// Maximum number of concurrently admitted local IPC sessions for one daemon.
pub const MAX_LOCAL_IPC_CONNECTIONS_V1: usize = 64;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "transport_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "transport_windows.rs"]
mod platform;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use std::time::Instant;

    use super::*;

    pub(super) struct Server;
    pub(super) struct Stream;
    pub(super) struct ReceivePrincipal;

    pub(super) fn bind(
        _namespace: &EndpointNamespaceV1,
        _instance: DaemonInstanceId,
    ) -> Result<Server, EndpointTransportError> {
        Err(EndpointTransportError::UnsupportedPlatform)
    }

    pub(super) async fn accept(
        _server: &mut Server,
    ) -> Result<(Stream, ProcessIdentityV1), EndpointTransportError> {
        Err(EndpointTransportError::UnsupportedPlatform)
    }

    pub(super) async fn connect(
        _namespace: &EndpointNamespaceV1,
        _discovered: DiscoveredEndpointV1,
        _deadline: Instant,
    ) -> Result<(Stream, ProcessIdentityV1), EndpointTransportError> {
        Err(EndpointTransportError::UnsupportedPlatform)
    }

    pub(super) fn begin_receive(
        _stream: &Stream,
        _expected: SecurityContextIdV1,
    ) -> Result<ReceivePrincipal, EndpointTransportError> {
        Err(EndpointTransportError::UnsupportedPlatform)
    }

    pub(super) fn finish_receive(
        _stream: &Stream,
        _expected: SecurityContextIdV1,
        _principal: ReceivePrincipal,
    ) -> Result<ProcessIdentityV1, EndpointTransportError> {
        Err(EndpointTransportError::UnsupportedPlatform)
    }
}

pub struct EndpointServerV1 {
    inner: platform::Server,
    daemon_instance_id: DaemonInstanceId,
    expected_security_context: SecurityContextIdV1,
}

impl EndpointServerV1 {
    pub(crate) fn bind_claimed(
        namespace: &EndpointNamespaceV1,
        lease: &DaemonLeaseV1,
        daemon_instance_id: DaemonInstanceId,
    ) -> Result<Self, EndpointTransportError> {
        lease.validate_namespace(namespace)?;
        if daemon_instance_id.as_bytes().iter().all(|byte| *byte == 0) {
            return Err(EndpointTransportError::ZeroDaemonInstanceId);
        }
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        tokio::runtime::Handle::try_current()
            .map_err(|_| EndpointTransportError::RuntimeUnavailable)?;
        Ok(Self {
            inner: platform::bind(namespace, daemon_instance_id)?,
            daemon_instance_id,
            expected_security_context: namespace.security_context_id(),
        })
    }

    #[must_use]
    pub const fn daemon_instance_id(&self) -> DaemonInstanceId {
        self.daemon_instance_id
    }

    pub async fn accept_verified(
        &mut self,
    ) -> Result<VerifiedFramedTransportV1, EndpointTransportError> {
        let (inner, peer_identity) = platform::accept(&mut self.inner).await?;
        Ok(VerifiedFramedTransportV1 {
            inner,
            peer_identity,
            expected_security_context: self.expected_security_context,
        })
    }
}

impl DiscoveredEndpointV1 {
    pub async fn connect_verified(
        self,
        namespace: &EndpointNamespaceV1,
        deadline: Instant,
    ) -> Result<VerifiedFramedTransportV1, EndpointTransportError> {
        let (inner, peer_identity) = platform::connect(namespace, self, deadline).await?;
        Ok(VerifiedFramedTransportV1 {
            inner,
            peer_identity,
            expected_security_context: namespace.security_context_id(),
        })
    }
}

/// Principal-bound local transport that exposes only bounded framed operations.
pub struct VerifiedFramedTransportV1 {
    inner: platform::Stream,
    peer_identity: ProcessIdentityV1,
    expected_security_context: SecurityContextIdV1,
}

impl VerifiedFramedTransportV1 {
    #[must_use]
    pub const fn peer_identity(&self) -> ProcessIdentityV1 {
        self.peer_identity
    }

    /// Reads one complete frame and verifies the latest platform-bound peer proof before returning
    /// its bytes. On Windows, server reads use message-level client impersonation; client reads
    /// revalidate the named-pipe server process and primary-token identity before receiving.
    pub async fn read_frame(
        &mut self,
        limits: FrameLimits,
        timeouts: FrameReadTimeoutsV1,
    ) -> Result<Option<Vec<u8>>, EndpointTransportError> {
        let principal = platform::begin_receive(&self.inner, self.expected_security_context)?;
        let frame = read_frame_from(
            &mut self.inner,
            limits.max_encoded_bytes(),
            timeouts.header,
            timeouts.body,
        )
        .await?;
        if frame.is_some() {
            let current_peer =
                platform::finish_receive(&self.inner, self.expected_security_context, principal)?;
            if current_peer != self.peer_identity {
                return Err(EndpointTransportError::PeerIdentityMismatch);
            }
        }
        Ok(frame)
    }

    /// Writes one already encoded frame after validating its declared length and bound.
    pub async fn write_frame(
        &mut self,
        frame: &[u8],
        limits: FrameLimits,
        timeout: Duration,
    ) -> Result<(), EndpointTransportError> {
        validate_encoded_frame(frame, limits.max_encoded_bytes())?;
        write_frame_to(&mut self.inner, frame, timeout).await
    }

    /// Runs admitted work while observing whether the peer closes or pipelines another request.
    ///
    /// The future always completes. Peer I/O failure is treated as closure so connection loss
    /// cannot cancel already admitted process-lifetime work.
    pub async fn monitor_inbound_while<F>(&mut self, future: F) -> (F::Output, FramedPeerStateV1)
    where
        F: Future,
    {
        tokio::pin!(future);
        let mut pipeline_byte = [0_u8; 1];
        tokio::select! {
            biased;
            read = self.inner.read(&mut pipeline_byte) => {
                let state = peer_state_from_read(read).unwrap_or(FramedPeerStateV1::Closed);
                (future.await, state)
            }
            output = &mut future => (output, FramedPeerStateV1::Open),
        }
    }

    /// Writes a response while preserving the peer state observed during domain dispatch.
    pub async fn write_frame_monitoring_inbound(
        &mut self,
        frame: &[u8],
        limits: FrameLimits,
        timeout: Duration,
        peer_state: FramedPeerStateV1,
    ) -> Result<FramedPeerStateV1, EndpointTransportError> {
        validate_encoded_frame(frame, limits.max_encoded_bytes())?;
        write_frame_monitoring_inbound_to(&mut self.inner, frame, timeout, peer_state).await
    }
}

impl fmt::Debug for VerifiedFramedTransportV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedFramedTransportV1")
            .field("peer_identity", &self.peer_identity)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramedPeerStateV1 {
    Open,
    Closed,
    Pipelined,
}

/// Separate idle-header and in-progress-frame limits for one inbound frame.
///
/// `body` is one shared deadline for the remaining header bytes and the complete payload after
/// the first header byte arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameReadTimeoutsV1 {
    header: Duration,
    body: Duration,
}

impl FrameReadTimeoutsV1 {
    #[must_use]
    pub const fn new(header: Duration, body: Duration) -> Self {
        Self { header, body }
    }

    #[must_use]
    pub const fn uniform(timeout: Duration) -> Self {
        Self::new(timeout, timeout)
    }
}

async fn read_frame_from(
    stream: &mut (impl AsyncRead + Unpin),
    maximum_encoded_bytes: usize,
    header_timeout: Duration,
    body_timeout: Duration,
) -> Result<Option<Vec<u8>>, EndpointTransportError> {
    let mut header = [0_u8; 4];
    let first = tokio::time::timeout(header_timeout, stream.read(&mut header[..1]))
        .await
        .map_err(|_| EndpointTransportError::FrameReadDeadlineElapsed)?
        .map_err(|source| EndpointTransportError::io("read local IPC frame header", source))?;
    if first == 0 {
        return Ok(None);
    }
    let body_deadline = tokio::time::Instant::now()
        .checked_add(body_timeout)
        .ok_or(EndpointTransportError::FrameDeadlineOverflow)?;
    tokio::time::timeout_at(body_deadline, stream.read_exact(&mut header[1..]))
        .await
        .map_err(|_| EndpointTransportError::FrameReadDeadlineElapsed)?
        .map_err(|source| EndpointTransportError::io("read local IPC frame header", source))?;
    let declared = u32::from_be_bytes(header) as usize;
    if declared > maximum_encoded_bytes {
        return Err(EndpointTransportError::FrameTooLarge {
            declared,
            maximum: maximum_encoded_bytes,
        });
    }
    let total = declared
        .checked_add(header.len())
        .ok_or(EndpointTransportError::FrameLengthOverflow)?;
    let mut frame = Vec::new();
    frame
        .try_reserve_exact(total)
        .map_err(|_| EndpointTransportError::FrameAllocationFailed { requested: total })?;
    frame.extend_from_slice(&header);
    frame.resize(total, 0);
    tokio::time::timeout_at(body_deadline, stream.read_exact(&mut frame[4..]))
        .await
        .map_err(|_| EndpointTransportError::FrameReadDeadlineElapsed)?
        .map_err(|source| EndpointTransportError::io("read local IPC frame body", source))?;
    Ok(Some(frame))
}

fn validate_encoded_frame(
    frame: &[u8],
    maximum_encoded_bytes: usize,
) -> Result<(), EndpointTransportError> {
    let header: [u8; 4] = frame
        .get(..4)
        .and_then(|header| header.try_into().ok())
        .ok_or(EndpointTransportError::InvalidEncodedFrame {
            reason: "encoded frame has no complete length header",
        })?;
    let declared = u32::from_be_bytes(header) as usize;
    if declared > maximum_encoded_bytes {
        return Err(EndpointTransportError::FrameTooLarge {
            declared,
            maximum: maximum_encoded_bytes,
        });
    }
    let actual = frame.len() - header.len();
    if declared != actual {
        return Err(EndpointTransportError::InvalidEncodedFrame {
            reason: "encoded frame length does not match its header",
        });
    }
    Ok(())
}

async fn write_frame_to(
    stream: &mut (impl AsyncWrite + Unpin),
    frame: &[u8],
    timeout: Duration,
) -> Result<(), EndpointTransportError> {
    tokio::time::timeout(timeout, async {
        stream.write_all(frame).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| EndpointTransportError::FrameWriteDeadlineElapsed)?
    .map_err(|source| EndpointTransportError::io("write local IPC frame", source))
}

async fn write_frame_monitoring_inbound_to(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    frame: &[u8],
    timeout: Duration,
    mut peer_state: FramedPeerStateV1,
) -> Result<FramedPeerStateV1, EndpointTransportError> {
    let deadline = tokio::time::Instant::now()
        .checked_add(timeout)
        .ok_or(EndpointTransportError::FrameDeadlineOverflow)?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut offset = 0;
    let mut pipeline_byte = [0_u8; 1];
    if peer_state == FramedPeerStateV1::Open {
        match probe_peer_state(&mut reader, &mut pipeline_byte).await? {
            Some(observed) => peer_state = observed,
            None => {
                // Windows starts an overlapped read on the first poll. Yield once so
                // already-buffered inbound bytes can complete before the response write becomes
                // the linearization point.
                tokio::task::yield_now().await;
            }
        }
    }
    while offset < frame.len() {
        if peer_state == FramedPeerStateV1::Closed {
            return Ok(peer_state);
        }
        if peer_state == FramedPeerStateV1::Open {
            if let Some(observed) = probe_peer_state(&mut reader, &mut pipeline_byte).await? {
                peer_state = observed;
                continue;
            }
            // A request that becomes readable before this write is submitted was already in
            // flight. Prefer the read when both operations are ready so delayed IOCP readiness
            // cannot turn a pipelined request into the next sequential exchange.
            tokio::select! {
                biased;
                read = reader.read(&mut pipeline_byte) => {
                    peer_state = peer_state_from_read(read)?;
                }
                written = tokio::time::timeout_at(deadline, writer.write(&frame[offset..])) => {
                    offset = advance_write(offset, written)?;
                }
            }
        } else {
            offset = advance_write(
                offset,
                tokio::time::timeout_at(deadline, writer.write(&frame[offset..])).await,
            )?;
        }
    }
    if peer_state == FramedPeerStateV1::Closed {
        return Ok(peer_state);
    }
    tokio::time::timeout_at(deadline, writer.flush())
        .await
        .map_err(|_| EndpointTransportError::FrameWriteDeadlineElapsed)?
        .map_err(|source| EndpointTransportError::io("flush local IPC frame", source))?;
    Ok(peer_state)
}

async fn probe_peer_state(
    reader: &mut (impl AsyncRead + Unpin),
    byte: &mut [u8; 1],
) -> Result<Option<FramedPeerStateV1>, EndpointTransportError> {
    std::future::poll_fn(|context| {
        let mut buffer = tokio::io::ReadBuf::new(byte);
        match std::pin::Pin::new(&mut *reader).poll_read(context, &mut buffer) {
            std::task::Poll::Pending => std::task::Poll::Ready(Ok(None)),
            std::task::Poll::Ready(Ok(())) => {
                std::task::Poll::Ready(peer_state_from_read(Ok(buffer.filled().len())).map(Some))
            }
            std::task::Poll::Ready(Err(source)) => std::task::Poll::Ready(Err(
                EndpointTransportError::io("monitor local IPC peer", source),
            )),
        }
    })
    .await
}

fn advance_write(
    offset: usize,
    result: Result<io::Result<usize>, tokio::time::error::Elapsed>,
) -> Result<usize, EndpointTransportError> {
    let written = result
        .map_err(|_| EndpointTransportError::FrameWriteDeadlineElapsed)?
        .map_err(|source| EndpointTransportError::io("write local IPC frame", source))?;
    if written == 0 {
        return Err(EndpointTransportError::io(
            "write local IPC frame",
            io::Error::new(
                io::ErrorKind::WriteZero,
                "local IPC frame write returned zero bytes",
            ),
        ));
    }
    offset
        .checked_add(written)
        .ok_or(EndpointTransportError::FrameLengthOverflow)
}

fn peer_state_from_read(
    result: io::Result<usize>,
) -> Result<FramedPeerStateV1, EndpointTransportError> {
    match result {
        Ok(0) => Ok(FramedPeerStateV1::Closed),
        Ok(_) => Ok(FramedPeerStateV1::Pipelined),
        Err(source) => Err(EndpointTransportError::io("monitor local IPC peer", source)),
    }
}

#[derive(Debug, Error)]
pub enum EndpointTransportError {
    #[error("local endpoint transport is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("local endpoint transport requires an active Tokio runtime")]
    RuntimeUnavailable,
    #[error("daemon instance ID must not be zero")]
    ZeroDaemonInstanceId,
    #[error("local endpoint name exceeds the platform address limit")]
    EndpointNameTooLong,
    #[error("the expected local endpoint is occupied")]
    EndpointCollision,
    #[error("the published local endpoint is no longer available")]
    EndpointUnavailable,
    #[error("the local endpoint is unsafe: {reason}")]
    UnsafeEndpoint { reason: &'static str },
    #[error("local endpoint connection deadline elapsed")]
    DeadlineElapsed,
    #[error("local IPC frame read deadline elapsed")]
    FrameReadDeadlineElapsed,
    #[error("local IPC frame write deadline elapsed")]
    FrameWriteDeadlineElapsed,
    #[error("local IPC frame deadline overflow")]
    FrameDeadlineOverflow,
    #[error("local IPC frame declared {declared} bytes; maximum is {maximum}")]
    FrameTooLarge { declared: usize, maximum: usize },
    #[error("local IPC frame length overflow")]
    FrameLengthOverflow,
    #[error("could not allocate {requested} bytes for a local IPC frame")]
    FrameAllocationFailed { requested: usize },
    #[error("invalid encoded local IPC frame: {reason}")]
    InvalidEncodedFrame { reason: &'static str },
    #[error("the operating system did not provide complete peer credentials")]
    PeerCredentialUnavailable,
    #[error("the connected peer has a different execution principal")]
    PeerContextMismatch,
    #[error("the connected local IPC peer changed process identity")]
    PeerIdentityMismatch,
    #[error("local IPC peer was rejected: {source}")]
    PeerRejected {
        #[source]
        source: Box<EndpointTransportError>,
    },
    #[error("local endpoint I/O failed while trying to {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Descriptor(#[from] EndpointDescriptorError),
    #[error(transparent)]
    Store(#[from] EndpointStoreError),
    #[error(transparent)]
    Process(#[from] ProcessIdentityError),
    #[error(transparent)]
    SecurityContext(#[from] SecurityContextError),
}

impl EndpointTransportError {
    pub(super) fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }

    pub(super) fn rejected_peer(source: Self) -> Self {
        Self::PeerRejected {
            source: Box::new(source),
        }
    }

    #[must_use]
    pub const fn is_peer_rejection(&self) -> bool {
        matches!(self, Self::PeerRejected { .. })
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};

    use super::{
        EndpointTransportError, FramedPeerStateV1, read_frame_from, validate_encoded_frame,
        write_frame_monitoring_inbound_to, write_frame_to,
    };

    const TEST_TIMEOUT: Duration = Duration::from_secs(1);

    fn encoded_frame(payload: &[u8]) -> Vec<u8> {
        let length = u32::try_from(payload.len()).expect("test payload length fits u32");
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    #[tokio::test]
    async fn bounded_frame_round_trip_preserves_exact_bytes() {
        let frame = encoded_frame(b"bounded frame");
        let (mut writer, mut reader) = tokio::io::duplex(128);
        let expected = frame.clone();
        let send = tokio::spawn(async move {
            write_frame_to(&mut writer, &frame, TEST_TIMEOUT)
                .await
                .expect("write test frame");
        });

        let actual = read_frame_from(&mut reader, 64, TEST_TIMEOUT, TEST_TIMEOUT)
            .await
            .expect("read test frame")
            .expect("frame is present");

        send.await.expect("join frame writer");
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn oversized_header_is_rejected_before_body_arrives() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer
            .write_all(&9_u32.to_be_bytes())
            .await
            .expect("write oversized header");

        let error = read_frame_from(&mut reader, 8, TEST_TIMEOUT, TEST_TIMEOUT)
            .await
            .expect_err("oversized frame must be rejected");

        assert!(matches!(
            error,
            EndpointTransportError::FrameTooLarge {
                declared: 9,
                maximum: 8
            }
        ));
    }

    #[tokio::test]
    async fn incomplete_body_obeys_the_body_deadline() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer
            .write_all(&2_u32.to_be_bytes())
            .await
            .expect("write partial frame header");
        writer
            .write_all(b"{")
            .await
            .expect("write partial frame body");

        let error = read_frame_from(&mut reader, 8, TEST_TIMEOUT, Duration::from_millis(20))
            .await
            .expect_err("partial body must time out");

        assert!(matches!(
            error,
            EndpointTransportError::FrameReadDeadlineElapsed
        ));
    }

    #[tokio::test]
    async fn header_and_payload_share_one_body_deadline() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        let send = tokio::spawn(async move {
            writer
                .write_all(&[0])
                .await
                .expect("write first frame header byte");
            tokio::time::sleep(Duration::from_millis(70)).await;
            writer
                .write_all(&[0, 0, 1])
                .await
                .expect("write remaining frame header bytes");
            tokio::time::sleep(Duration::from_millis(70)).await;
            let _ = writer.write_all(b"x").await;
        });

        let error = read_frame_from(&mut reader, 8, TEST_TIMEOUT, Duration::from_millis(100))
            .await
            .expect_err("header and payload must not receive separate body deadlines");

        assert!(matches!(
            error,
            EndpointTransportError::FrameReadDeadlineElapsed
        ));
        send.await.expect("join staggered frame writer");
    }

    #[tokio::test]
    async fn response_deadline_overflow_is_a_typed_error() {
        let frame = encoded_frame(b"response");
        let (mut server, _client) = tokio::io::duplex(128);

        let error = write_frame_monitoring_inbound_to(
            &mut server,
            &frame,
            Duration::MAX,
            FramedPeerStateV1::Open,
        )
        .await
        .expect_err("overflowing deadline must be rejected");

        assert!(matches!(
            error,
            EndpointTransportError::FrameDeadlineOverflow
        ));
    }

    #[test]
    fn outgoing_frame_requires_a_complete_matching_header() {
        assert!(matches!(
            validate_encoded_frame(b"abc", 8),
            Err(EndpointTransportError::InvalidEncodedFrame { .. })
        ));
        assert!(matches!(
            validate_encoded_frame(&[0, 0, 0, 2, b'x'], 8),
            Err(EndpointTransportError::InvalidEncodedFrame { .. })
        ));
        assert!(matches!(
            validate_encoded_frame(&encoded_frame(b"123456789"), 8),
            Err(EndpointTransportError::FrameTooLarge {
                declared: 9,
                maximum: 8
            })
        ));
    }

    #[tokio::test]
    async fn response_write_remains_open_without_inbound_bytes() {
        let frame = encoded_frame(b"response");
        let (mut server, mut client) = tokio::io::duplex(128);
        let expected = frame.clone();
        let receive = tokio::spawn(async move {
            let mut actual = vec![0_u8; expected.len()];
            client
                .read_exact(&mut actual)
                .await
                .expect("read monitored response");
            (actual, expected)
        });

        let state = write_frame_monitoring_inbound_to(
            &mut server,
            &frame,
            TEST_TIMEOUT,
            FramedPeerStateV1::Open,
        )
        .await
        .expect("write monitored response");
        let (actual, expected) = receive.await.expect("join response reader");

        assert_eq!(state, FramedPeerStateV1::Open);
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn buffered_inbound_byte_marks_response_as_pipelined() {
        let frame = encoded_frame(b"response");
        let (mut server, mut client) = tokio::io::duplex(128);
        client
            .write_all(b"x")
            .await
            .expect("buffer pipelined request byte");
        let expected = frame.clone();
        let receive = tokio::spawn(async move {
            let mut actual = vec![0_u8; expected.len()];
            client
                .read_exact(&mut actual)
                .await
                .expect("read response after pipeline detection");
            (actual, expected)
        });

        let state = write_frame_monitoring_inbound_to(
            &mut server,
            &frame,
            TEST_TIMEOUT,
            FramedPeerStateV1::Open,
        )
        .await
        .expect("write response after pipeline detection");
        let (actual, expected) = receive.await.expect("join response reader");

        assert_eq!(state, FramedPeerStateV1::Pipelined);
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn pending_platform_read_is_repolled_before_an_immediately_ready_write() {
        let frame = encoded_frame(b"response");
        let mut transport = DelayedInbound::new(2);

        let state = write_frame_monitoring_inbound_to(
            &mut transport,
            &frame,
            TEST_TIMEOUT,
            FramedPeerStateV1::Open,
        )
        .await
        .expect("write response after delayed inbound readiness");

        assert_eq!(state, FramedPeerStateV1::Pipelined);
        assert_eq!(transport.read_polls, 2);
        assert_eq!(transport.written, frame);
    }

    #[tokio::test]
    async fn delayed_inbound_request_wins_over_a_ready_response_write() {
        let frame = encoded_frame(b"response");
        let mut transport = DelayedInbound::new(3);

        let state = write_frame_monitoring_inbound_to(
            &mut transport,
            &frame,
            TEST_TIMEOUT,
            FramedPeerStateV1::Open,
        )
        .await
        .expect("write response at the sequential-request boundary");

        assert_eq!(state, FramedPeerStateV1::Pipelined);
        assert_eq!(transport.read_polls, 3);
        assert_eq!(transport.written, frame);
    }

    #[tokio::test]
    async fn closed_peer_suppresses_response_write() {
        let frame = encoded_frame(b"response");
        let (mut server, client) = tokio::io::duplex(128);
        drop(client);

        let state = write_frame_monitoring_inbound_to(
            &mut server,
            &frame,
            TEST_TIMEOUT,
            FramedPeerStateV1::Open,
        )
        .await
        .expect("closed peer is an observed state");

        assert_eq!(state, FramedPeerStateV1::Closed);
    }

    struct DelayedInbound {
        readable_on_poll: usize,
        read_polls: usize,
        written: Vec<u8>,
    }

    impl DelayedInbound {
        fn new(readable_on_poll: usize) -> Self {
            Self {
                readable_on_poll,
                read_polls: 0,
                written: Vec::new(),
            }
        }
    }

    impl AsyncRead for DelayedInbound {
        fn poll_read(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            self.read_polls += 1;
            if self.read_polls < self.readable_on_poll {
                context.waker().wake_by_ref();
                return Poll::Pending;
            }
            buffer.put_slice(b"x");
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for DelayedInbound {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.written.extend_from_slice(buffer);
            Poll::Ready(Ok(buffer.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}
