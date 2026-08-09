use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

use tokio::net::{UnixListener, UnixStream};
use unity_asset_search_protocol::DaemonInstanceId;

use super::EndpointTransportError;
use crate::{DiscoveredEndpointV1, EndpointNamespaceV1, ProcessIdentityV1, SecurityContextIdV1};

const SOCKET_FILE: &str = "daemon.sock";
const SOCKET_MODE: u32 = 0o600;

pub(super) struct Server {
    listener: UnixListener,
    socket_path: PathBuf,
    socket_identity: SocketIdentity,
    security_context_id: SecurityContextIdV1,
}

pub(super) struct Stream {
    inner: UnixStream,
}

pub(super) struct ReceivePrincipal;

pub(super) fn bind(
    namespace: &EndpointNamespaceV1,
    _instance: DaemonInstanceId,
) -> Result<Server, EndpointTransportError> {
    namespace.revalidate().map_err(|source| {
        EndpointTransportError::io("revalidate endpoint namespace", io::Error::other(source))
    })?;
    let socket_path = namespace.path().join(SOCKET_FILE);
    rustix::net::SocketAddrUnix::new(&socket_path)
        .map_err(|_| EndpointTransportError::EndpointNameTooLong)?;
    prepare_socket_path(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)
        .map_err(|source| EndpointTransportError::io("bind Unix endpoint", source))?;
    if let Err(source) = fs::set_permissions(&socket_path, fs::Permissions::from_mode(SOCKET_MODE))
    {
        drop(listener);
        let _ = fs::remove_file(&socket_path);
        return Err(EndpointTransportError::io(
            "set Unix endpoint permissions",
            source,
        ));
    }
    let socket_identity = validate_socket(&socket_path)?;
    Ok(Server {
        listener,
        socket_path,
        socket_identity,
        security_context_id: namespace.security_context_id(),
    })
}

pub(super) async fn accept(
    server: &mut Server,
) -> Result<(Stream, ProcessIdentityV1), EndpointTransportError> {
    let (inner, _) = server
        .listener
        .accept()
        .await
        .map_err(|source| EndpointTransportError::io("accept Unix endpoint peer", source))?;
    let peer = verify_peer(&inner, server.security_context_id)
        .map_err(EndpointTransportError::rejected_peer)?;
    Ok((Stream { inner }, peer))
}

pub(super) async fn connect(
    namespace: &EndpointNamespaceV1,
    discovered: DiscoveredEndpointV1,
    deadline: Instant,
) -> Result<(Stream, ProcessIdentityV1), EndpointTransportError> {
    ensure_deadline(deadline)?;
    let socket_path = namespace.path().join(SOCKET_FILE);
    rustix::net::SocketAddrUnix::new(&socket_path)
        .map_err(|_| EndpointTransportError::EndpointNameTooLong)?;
    let before = match validate_socket(&socket_path) {
        Ok(identity) => identity,
        Err(EndpointTransportError::Io { source, .. })
            if source.kind() == io::ErrorKind::NotFound =>
        {
            return Err(EndpointTransportError::EndpointUnavailable);
        }
        Err(error) => return Err(error),
    };
    ensure_deadline(deadline)?;
    let inner = tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        UnixStream::connect(&socket_path),
    )
    .await
    .map_err(|_| EndpointTransportError::DeadlineElapsed)?
    .map_err(|source| {
        if matches!(
            source.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        ) {
            EndpointTransportError::EndpointUnavailable
        } else {
            EndpointTransportError::io("connect Unix endpoint", source)
        }
    })?;
    let process_id = verify_peer_credentials(&inner, namespace.security_context_id())?;
    ensure_deadline(deadline)?;
    let validation_namespace = namespace.clone();
    let validation_path = socket_path.clone();
    let expected_context = namespace.security_context_id();
    let validation = tokio::task::spawn_blocking(move || {
        let peer = inspect_peer_process(process_id, expected_context)?;
        if let Err(error) = discovered.descriptor().validate_server_process(peer) {
            discovered.ensure_unchanged(&validation_namespace)?;
            return Err(error.into());
        }
        if validate_socket(&validation_path)? != before {
            return Err(EndpointTransportError::Store(
                crate::EndpointStoreError::EndpointChanged,
            ));
        }
        discovered.ensure_unchanged(&validation_namespace)?;
        ensure_deadline(deadline)?;
        Ok(peer)
    });
    let peer = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), validation)
        .await
        .map_err(|_| EndpointTransportError::DeadlineElapsed)?
        .map_err(|source| {
            EndpointTransportError::io(
                "join Unix endpoint identity validation",
                io::Error::other(source),
            )
        })??;
    Ok((Stream { inner }, peer))
}

pub(super) fn begin_receive(
    _stream: &Stream,
    _expected: SecurityContextIdV1,
) -> Result<ReceivePrincipal, EndpointTransportError> {
    Ok(ReceivePrincipal)
}

pub(super) fn finish_receive(
    stream: &Stream,
    expected: SecurityContextIdV1,
    _principal: ReceivePrincipal,
) -> Result<ProcessIdentityV1, EndpointTransportError> {
    verify_peer(&stream.inner, expected)
}

fn prepare_socket_path(path: &Path) -> Result<(), EndpointTransportError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(EndpointTransportError::io(
                "inspect existing Unix endpoint",
                source,
            ));
        }
    };
    validate_socket_metadata(&metadata)?;
    fs::remove_file(path)
        .map_err(|source| EndpointTransportError::io("claim stale Unix endpoint", source))
}

fn verify_peer(
    stream: &UnixStream,
    expected: SecurityContextIdV1,
) -> Result<ProcessIdentityV1, EndpointTransportError> {
    let process_id = verify_peer_credentials(stream, expected)?;
    inspect_peer_process(process_id, expected)
}

fn verify_peer_credentials(
    stream: &UnixStream,
    expected: SecurityContextIdV1,
) -> Result<u32, EndpointTransportError> {
    let credentials = stream
        .peer_cred()
        .map_err(|source| EndpointTransportError::io("read Unix peer credentials", source))?;
    let process_id = credentials
        .pid()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
        .ok_or(EndpointTransportError::PeerCredentialUnavailable)?;
    let effective_uid = u32::try_from(credentials.uid())
        .map_err(|_| EndpointTransportError::PeerCredentialUnavailable)?;
    if SecurityContextIdV1::for_effective_uid(effective_uid)? != expected {
        return Err(EndpointTransportError::PeerContextMismatch);
    }
    Ok(process_id)
}

fn inspect_peer_process(
    process_id: u32,
    expected: SecurityContextIdV1,
) -> Result<ProcessIdentityV1, EndpointTransportError> {
    let process = ProcessIdentityV1::inspect(process_id)?;
    if process.security_context_id() != expected {
        return Err(EndpointTransportError::PeerContextMismatch);
    }
    Ok(process)
}

fn ensure_deadline(deadline: Instant) -> Result<(), EndpointTransportError> {
    if Instant::now() >= deadline {
        Err(EndpointTransportError::DeadlineElapsed)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

fn validate_socket(path: &Path) -> Result<SocketIdentity, EndpointTransportError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| EndpointTransportError::io("inspect Unix endpoint", source))?;
    validate_socket_metadata(&metadata)?;
    let device = metadata.dev();
    let inode = metadata.ino();
    if device == 0 || inode == 0 {
        return Err(EndpointTransportError::UnsafeEndpoint {
            reason: "socket has no stable file identity",
        });
    }
    Ok(SocketIdentity { device, inode })
}

fn validate_socket_metadata(metadata: &fs::Metadata) -> Result<(), EndpointTransportError> {
    if !metadata.file_type().is_socket() {
        return Err(EndpointTransportError::UnsafeEndpoint {
            reason: "endpoint leaf is not a Unix socket",
        });
    }
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != SOCKET_MODE
    {
        return Err(EndpointTransportError::UnsafeEndpoint {
            reason: "Unix socket is not owner-only",
        });
    }
    Ok(())
}

impl Drop for Server {
    fn drop(&mut self) {
        if validate_socket(&self.socket_path).ok() == Some(self.socket_identity) {
            let _ = fs::remove_file(&self.socket_path);
        }
    }
}

impl tokio::io::AsyncRead for Stream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl tokio::io::AsyncWrite for Stream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(context)
    }
}
