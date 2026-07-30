use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use unity_asset_search_protocol::DaemonInstanceId;

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

    pub(super) fn verify_received_message(
        _stream: &Stream,
        _expected: SecurityContextIdV1,
    ) -> Result<(), EndpointTransportError> {
        Err(EndpointTransportError::UnsupportedPlatform)
    }
}

pub struct EndpointServerV1 {
    inner: platform::Server,
    daemon_instance_id: DaemonInstanceId,
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
        })
    }

    #[must_use]
    pub const fn daemon_instance_id(&self) -> DaemonInstanceId {
        self.daemon_instance_id
    }

    pub async fn accept_verified(
        &mut self,
    ) -> Result<VerifiedLocalStreamV1, EndpointTransportError> {
        let (inner, peer_identity) = platform::accept(&mut self.inner).await?;
        Ok(VerifiedLocalStreamV1 {
            inner,
            peer_identity,
        })
    }
}

impl DiscoveredEndpointV1 {
    pub async fn connect_verified(
        self,
        namespace: &EndpointNamespaceV1,
        deadline: Instant,
    ) -> Result<VerifiedLocalStreamV1, EndpointTransportError> {
        let (inner, peer_identity) = platform::connect(namespace, self, deadline).await?;
        Ok(VerifiedLocalStreamV1 {
            inner,
            peer_identity,
        })
    }
}

pub struct VerifiedLocalStreamV1 {
    inner: platform::Stream,
    peer_identity: ProcessIdentityV1,
}

impl VerifiedLocalStreamV1 {
    #[must_use]
    pub const fn peer_identity(&self) -> ProcessIdentityV1 {
        self.peer_identity
    }

    pub fn verify_received_message_principal(
        &self,
        expected: SecurityContextIdV1,
    ) -> Result<(), EndpointTransportError> {
        platform::verify_received_message(&self.inner, expected)
    }
}

impl fmt::Debug for VerifiedLocalStreamV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedLocalStreamV1")
            .field("peer_identity", &self.peer_identity)
            .finish_non_exhaustive()
    }
}

impl AsyncRead for VerifiedLocalStreamV1 {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(context, buffer)
    }
}

impl AsyncWrite for VerifiedLocalStreamV1 {
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
    #[error("the operating system did not provide complete peer credentials")]
    PeerCredentialUnavailable,
    #[error("the connected peer has a different execution principal")]
    PeerContextMismatch,
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
