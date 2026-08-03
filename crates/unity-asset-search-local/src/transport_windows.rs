use std::ffi::OsStr;
use std::io;
use std::iter;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::time::{Duration, Instant};

use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
use tokio::task::JoinSet;
use unity_asset_search_protocol::DaemonInstanceId;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{RevertToSelf, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, ImpersonateNamedPipeClient,
};

use super::EndpointTransportError;
use crate::pipe_rendezvous::{PipeRendezvousV1, PipeSlotId, PublishedPipeRendezvousV1, discover};
use crate::roots::{WINDOWS_NAMED_PIPE_CLIENT_ACCESS, WindowsPrivateSecurityDescriptor};
use crate::security_context::CurrentSecurityContextSnapshot;
use crate::{DiscoveredEndpointV1, EndpointNamespaceV1, ProcessIdentityV1, SecurityContextIdV1};

const PIPE_PREFIX: &str = r"\\.\pipe\LOCAL\uas-v1-";
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;
const BUSY_RETRY: Duration = Duration::from_millis(10);
const SLOT_CREATION_ATTEMPTS: usize = 16;

pub(super) struct Server {
    // Drop removes the rendezvous before JoinSet aborts and closes the pending pipe.
    publication: Option<PublishedPipeRendezvousV1>,
    pending: JoinSet<(NamedPipeServer, io::Result<()>)>,
    pipe_base: String,
    security: WindowsPrivateSecurityDescriptor,
    security_context_id: SecurityContextIdV1,
}

pub(super) struct Stream {
    inner: PipeStream,
    server_side: bool,
}

enum PipeStream {
    Server(NamedPipeServer),
    Client(NamedPipeClient),
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let PipeStream::Server(pipe) = &self.inner {
            // This object is never reused. Disconnect forces any pending Mio write to complete
            // instead of retaining the pipe handle and response buffer beyond the session permit.
            let _ = pipe.disconnect();
        }
    }
}

impl Server {
    fn arm(&mut self, pipe: NamedPipeServer) {
        debug_assert!(self.pending.is_empty());
        self.pending.spawn(async move {
            let connected = pipe.connect().await;
            (pipe, connected)
        });
    }

    fn rotate(&mut self) -> Result<(), EndpointTransportError> {
        let current = self
            .publication
            .as_ref()
            .expect("Windows server retains rendezvous publication")
            .current();
        let (next, pipe) = create_successor_slot(&self.pipe_base, current, &self.security)?;
        self.publication
            .as_mut()
            .expect("Windows server retains rendezvous publication")
            .rotate(next)?;
        self.arm(pipe);
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(publication) = self.publication.take() {
            let _ = publication.remove();
        }
        self.pending.abort_all();
    }
}

pub(super) fn bind(
    namespace: &EndpointNamespaceV1,
    instance: DaemonInstanceId,
) -> Result<Server, EndpointTransportError> {
    namespace.revalidate().map_err(|source| {
        EndpointTransportError::io("revalidate endpoint namespace", io::Error::other(source))
    })?;
    let snapshot = CurrentSecurityContextSnapshot::current()?;
    if snapshot.id() != namespace.security_context_id() {
        return Err(EndpointTransportError::PeerContextMismatch);
    }
    let security = WindowsPrivateSecurityDescriptor::for_named_pipe(
        snapshot.windows_user_sid(),
        snapshot.windows_logon_sid(),
    )
    .map_err(|source| {
        EndpointTransportError::io("construct Windows named-pipe client descriptor", source)
    })?;
    let pipe_base = pipe_base_name(namespace, instance);
    let (initial, pipe) = create_initial_slot(namespace, instance, &pipe_base, &security)?;
    let publication = PublishedPipeRendezvousV1::publish(namespace, initial)?;
    let mut server = Server {
        publication: Some(publication),
        pending: JoinSet::new(),
        pipe_base,
        security,
        security_context_id: namespace.security_context_id(),
    };
    server.arm(pipe);
    Ok(server)
}

pub(super) async fn accept(
    server: &mut Server,
) -> Result<(Stream, ProcessIdentityV1), EndpointTransportError> {
    let (connected, result) = server
        .pending
        .join_next()
        .await
        .ok_or_else(|| {
            EndpointTransportError::io(
                "accept Windows named-pipe slot",
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "named-pipe accept task is missing",
                ),
            )
        })?
        .map_err(|source| {
            EndpointTransportError::io(
                "join Windows named-pipe accept task",
                io::Error::other(source),
            )
        })?;

    // Every observed connection consumes this one-shot Tokio/Mio object. Publish and arm a fresh
    // random slot before returning either a valid stream or a peer-rejection error.
    server.rotate()?;
    result.map_err(|source| {
        EndpointTransportError::rejected_peer(EndpointTransportError::io(
            "accept Windows named-pipe peer",
            source,
        ))
    })?;

    let peer = inspect_connected_peer(&connected, server.security_context_id)
        .map_err(EndpointTransportError::rejected_peer)?;
    Ok((
        Stream {
            inner: PipeStream::Server(connected),
            server_side: true,
        },
        peer,
    ))
}

pub(super) async fn connect(
    namespace: &EndpointNamespaceV1,
    discovered: DiscoveredEndpointV1,
    deadline: Instant,
) -> Result<(Stream, ProcessIdentityV1), EndpointTransportError> {
    let descriptor = discovered.descriptor();
    let pipe_base = pipe_base_name(namespace, descriptor.daemon_instance_id());

    loop {
        if Instant::now() >= deadline {
            return Err(EndpointTransportError::DeadlineElapsed);
        }
        discovered.ensure_unchanged(namespace)?;
        let rendezvous = match discover(namespace, descriptor) {
            Ok(rendezvous) => rendezvous,
            Err(error) => {
                discovered.ensure_unchanged(namespace)?;
                return Err(error);
            }
        };
        let pipe_name = pipe_slot_name(&pipe_base, rendezvous.slot_id());
        match open_client_pipe(&pipe_name) {
            Ok(client) => {
                let validation = validate_connected_server(namespace, &discovered, &client);
                match validation {
                    Ok(peer) => {
                        if Instant::now() >= deadline {
                            return Err(EndpointTransportError::DeadlineElapsed);
                        }
                        return Ok((
                            Stream {
                                inner: PipeStream::Client(client),
                                server_side: false,
                            },
                            peer,
                        ));
                    }
                    Err(error) => {
                        drop(client);
                        discovered.ensure_unchanged(namespace)?;
                        if discover(namespace, descriptor)
                            .is_ok_and(|current| current != rendezvous)
                        {
                            continue;
                        }
                        return Err(error);
                    }
                }
            }
            Err(source)
                if source.raw_os_error()
                    == Some(i32::try_from(ERROR_PIPE_BUSY).expect("Win32 code fits i32")) =>
            {
                wait_for_rotation(namespace, descriptor, rendezvous, deadline).await?;
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                discovered.ensure_unchanged(namespace)?;
                if discover(namespace, descriptor).is_ok_and(|current| current != rendezvous) {
                    continue;
                }
                return Err(EndpointTransportError::EndpointUnavailable);
            }
            Err(source)
                if source.raw_os_error()
                    == Some(i32::try_from(ERROR_ACCESS_DENIED).expect("Win32 code fits i32")) =>
            {
                discovered.ensure_unchanged(namespace)?;
                if discover(namespace, descriptor).is_ok_and(|current| current != rendezvous) {
                    continue;
                }
                return Err(EndpointTransportError::UnsafeEndpoint {
                    reason: "published Windows pipe slot denied the required minimum client access",
                });
            }
            Err(source) => {
                discovered.ensure_unchanged(namespace)?;
                return Err(EndpointTransportError::io(
                    "connect Windows named-pipe slot",
                    source,
                ));
            }
        }
    }
}

async fn wait_for_rotation(
    namespace: &EndpointNamespaceV1,
    descriptor: crate::EndpointDescriptorV1,
    observed: PipeRendezvousV1,
    deadline: Instant,
) -> Result<(), EndpointTransportError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(EndpointTransportError::DeadlineElapsed);
    }
    if discover(namespace, descriptor).is_ok_and(|current| current != observed) {
        return Ok(());
    }
    tokio::time::sleep(BUSY_RETRY.min(deadline.saturating_duration_since(now))).await;
    Ok(())
}

fn inspect_connected_peer(
    pipe: &NamedPipeServer,
    expected_context: SecurityContextIdV1,
) -> Result<ProcessIdentityV1, EndpointTransportError> {
    let process_id = named_pipe_process_id(pipe.as_raw_handle().cast(), true)?;
    let peer = ProcessIdentityV1::inspect(process_id)?;
    if peer.security_context_id() != expected_context {
        return Err(EndpointTransportError::PeerContextMismatch);
    }
    Ok(peer)
}

fn validate_connected_server(
    namespace: &EndpointNamespaceV1,
    discovered: &DiscoveredEndpointV1,
    client: &NamedPipeClient,
) -> Result<ProcessIdentityV1, EndpointTransportError> {
    let process_id = named_pipe_process_id(client.as_raw_handle().cast(), false)?;
    let peer = ProcessIdentityV1::inspect(process_id)?;
    discovered.descriptor().validate_server_process(peer)?;
    if peer.security_context_id() != namespace.security_context_id() {
        return Err(EndpointTransportError::PeerContextMismatch);
    }
    discovered.ensure_unchanged(namespace)?;
    Ok(peer)
}

fn create_initial_slot(
    namespace: &EndpointNamespaceV1,
    instance: DaemonInstanceId,
    pipe_base: &str,
    security: &WindowsPrivateSecurityDescriptor,
) -> Result<(PipeRendezvousV1, NamedPipeServer), EndpointTransportError> {
    for _ in 0..SLOT_CREATION_ATTEMPTS {
        let slot_id = PipeSlotId::generate()?;
        let rendezvous = PipeRendezvousV1::initial(namespace, instance, slot_id)?;
        match create_verified_server_pipe(pipe_base, slot_id, security) {
            Ok(pipe) => return Ok((rendezvous, pipe)),
            Err(EndpointTransportError::EndpointCollision) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(EndpointTransportError::EndpointCollision)
}

fn create_successor_slot(
    pipe_base: &str,
    current: PipeRendezvousV1,
    security: &WindowsPrivateSecurityDescriptor,
) -> Result<(PipeRendezvousV1, NamedPipeServer), EndpointTransportError> {
    for _ in 0..SLOT_CREATION_ATTEMPTS {
        let slot_id = PipeSlotId::generate()?;
        let next = current.next(slot_id)?;
        match create_verified_server_pipe(pipe_base, slot_id, security) {
            Ok(pipe) => return Ok((next, pipe)),
            Err(EndpointTransportError::EndpointCollision) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(EndpointTransportError::EndpointCollision)
}

fn create_verified_server_pipe(
    pipe_base: &str,
    slot_id: PipeSlotId,
    security: &WindowsPrivateSecurityDescriptor,
) -> Result<NamedPipeServer, EndpointTransportError> {
    let pipe = create_server_pipe(&pipe_slot_name(pipe_base, slot_id), security)?;
    security
        .verify_handle(pipe.as_raw_handle().cast())
        .map_err(|source| EndpointTransportError::io("verify Windows named-pipe DACL", source))?;
    Ok(pipe)
}

fn pipe_base_name(namespace: &EndpointNamespaceV1, instance: DaemonInstanceId) -> String {
    format!(
        "{PIPE_PREFIX}{}-{}-",
        namespace.component(),
        hex::encode(instance.as_bytes())
    )
}

fn pipe_slot_name(pipe_base: &str, slot_id: PipeSlotId) -> String {
    format!("{pipe_base}{}", hex::encode(slot_id.as_bytes()))
}

pub(super) fn verify_received_message(
    stream: &Stream,
    expected: SecurityContextIdV1,
) -> Result<(), EndpointTransportError> {
    if !stream.server_side {
        return Ok(());
    }
    let PipeStream::Server(pipe) = &stream.inner else {
        return Err(EndpointTransportError::PeerCredentialUnavailable);
    };
    let handle = pipe.as_raw_handle().cast::<std::ffi::c_void>();
    // The caller invokes this only after one complete bounded message has been read, which is when
    // Windows fixes the message principal used by ImpersonateNamedPipeClient.
    if unsafe { ImpersonateNamedPipeClient(handle) } == 0 {
        return Err(EndpointTransportError::io(
            "impersonate Windows named-pipe message principal",
            io::Error::last_os_error(),
        ));
    }
    let revert = RevertGuard { active: true };
    let actual = SecurityContextIdV1::for_impersonated_thread()?;
    revert.revert();
    if actual != expected {
        return Err(EndpointTransportError::PeerContextMismatch);
    }
    Ok(())
}

fn create_server_pipe(
    pipe_name: &str,
    security: &WindowsPrivateSecurityDescriptor,
) -> Result<NamedPipeServer, EndpointTransportError> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
            .expect("SECURITY_ATTRIBUTES size fits u32"),
        lpSecurityDescriptor: security.as_ptr().cast_mut().cast(),
        bInheritHandle: 0,
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .max_instances(1)
        .in_buffer_size(PIPE_BUFFER_BYTES)
        .out_buffer_size(PIPE_BUFFER_BYTES);
    // The descriptor and all allocations it references outlive this create call. Windows copies
    // the descriptor into the new kernel object before returning.
    unsafe { options.create_with_security_attributes_raw(pipe_name, (&raw mut attributes).cast()) }
        .map_err(|source| {
            if source.raw_os_error()
                == Some(i32::try_from(ERROR_ACCESS_DENIED).expect("Win32 code fits i32"))
            {
                EndpointTransportError::EndpointCollision
            } else {
                EndpointTransportError::io("create one-shot Windows named-pipe slot", source)
            }
        })
}

fn open_client_pipe(pipe_name: &str) -> io::Result<NamedPipeClient> {
    open_client_pipe_with_access(pipe_name, WINDOWS_NAMED_PIPE_CLIENT_ACCESS)
}

fn open_client_pipe_with_access(
    pipe_name: &str,
    desired_access: u32,
) -> io::Result<NamedPipeClient> {
    let units = OsStr::new(pipe_name).encode_wide().count();
    let capacity = units
        .checked_add(1)
        .ok_or_else(|| io::Error::other("Windows named-pipe path length overflow"))?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(capacity)
        .map_err(|_| io::Error::other("Windows named-pipe path allocation failed"))?;
    encoded.extend(OsStr::new(pipe_name).encode_wide().chain(iter::once(0)));
    let handle = unsafe {
        CreateFileW(
            encoded.as_ptr(),
            desired_access,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_IDENTIFICATION | SECURITY_SQOS_PRESENT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // CreateFileW returned a live, uniquely owned, overlapped named-pipe handle.
    unsafe { NamedPipeClient::from_raw_handle(handle.cast()) }
}

fn named_pipe_process_id(handle: HANDLE, client: bool) -> Result<u32, EndpointTransportError> {
    let mut process_id = 0_u32;
    let succeeded = if client {
        unsafe { GetNamedPipeClientProcessId(handle, &raw mut process_id) }
    } else {
        unsafe { GetNamedPipeServerProcessId(handle, &raw mut process_id) }
    };
    if succeeded == 0 {
        return Err(EndpointTransportError::io(
            "read Windows named-pipe peer process ID",
            io::Error::last_os_error(),
        ));
    }
    if process_id == 0 {
        return Err(EndpointTransportError::PeerCredentialUnavailable);
    }
    Ok(process_id)
}

struct RevertGuard {
    active: bool,
}

impl RevertGuard {
    fn revert(mut self) {
        // Continuing a Tokio worker after a failed revert would execute unrelated work under the
        // client's token. There is no safe in-process recovery from that thread contamination.
        if unsafe { RevertToSelf() } == 0 {
            std::process::abort();
        }
        self.active = false;
    }
}

impl Drop for RevertGuard {
    fn drop(&mut self) {
        if self.active && unsafe { RevertToSelf() } == 0 {
            std::process::abort();
        }
    }
}

impl Stream {
    fn inner_mut(&mut self) -> &mut PipeStream {
        &mut self.inner
    }
}

impl tokio::io::AsyncRead for Stream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.inner_mut() {
            PipeStream::Server(stream) => std::pin::Pin::new(stream).poll_read(context, buffer),
            PipeStream::Client(stream) => std::pin::Pin::new(stream).poll_read(context, buffer),
        }
    }
}

impl tokio::io::AsyncWrite for Stream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        match self.inner_mut() {
            PipeStream::Server(stream) => std::pin::Pin::new(stream).poll_write(context, buffer),
            PipeStream::Client(stream) => std::pin::Pin::new(stream).poll_write(context, buffer),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.inner_mut() {
            PipeStream::Server(stream) => std::pin::Pin::new(stream).poll_flush(context),
            PipeStream::Client(stream) => std::pin::Pin::new(stream).poll_flush(context),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.inner_mut() {
            PipeStream::Server(stream) => std::pin::Pin::new(stream).poll_shutdown(context),
            PipeStream::Client(stream) => std::pin::Pin::new(stream).poll_shutdown(context),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::{Duration, Instant};

    use tokio::io::AsyncReadExt as _;
    use tokio::net::windows::named_pipe::ServerOptions;
    use unity_asset_search_protocol::{FrameLimits, ProjectId};
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;

    use super::{
        WINDOWS_NAMED_PIPE_CLIENT_ACCESS, accept, bind, create_server_pipe, open_client_pipe,
        open_client_pipe_with_access, pipe_base_name, pipe_slot_name,
    };
    use crate::pipe_rendezvous::discover;
    use crate::security_context::CurrentSecurityContextSnapshot;
    use crate::{EndpointDescriptorV1, PrivateRootsV1, generate_daemon_instance_id};

    fn unique_project_id() -> ProjectId {
        let mut bytes = rand::random::<[u8; 32]>();
        bytes[0] |= 1;
        ProjectId::from_bytes(bytes)
    }

    fn cleanup_namespace(path: &std::path::Path) {
        for name in [
            "binding.v1",
            ".binding-v1.lock",
            ".daemon-v1.lock",
            "windows-pipe-slot.v1.json",
        ] {
            let result = std::fs::remove_file(path.join(name));
            assert!(
                result.is_ok()
                    || result.is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
            );
        }
        std::fs::remove_dir(path).unwrap();
    }

    fn raw_pipe_name(label: &str) -> String {
        let suffix = hex::encode(rand::random::<[u8; 16]>());
        format!(r"\\.\pipe\LOCAL\uas-v1-{label}-{suffix}")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn final_client_dacl_never_grants_server_or_acl_mutation_rights() {
        let pipe_name = raw_pipe_name("security-test");
        let context = CurrentSecurityContextSnapshot::current().unwrap();
        let security = crate::roots::WindowsPrivateSecurityDescriptor::for_named_pipe(
            context.windows_user_sid(),
            context.windows_logon_sid(),
        )
        .unwrap();
        let pipe = create_server_pipe(&pipe_name, &security).unwrap();

        let mut options = ServerOptions::new();
        options.first_pipe_instance(true).max_instances(1);
        let error = options.create(&pipe_name).unwrap_err();
        assert_eq!(
            error.raw_os_error(),
            Some(i32::try_from(ERROR_ACCESS_DENIED).unwrap())
        );
        let error =
            open_client_pipe_with_access(&pipe_name, WINDOWS_NAMED_PIPE_CLIENT_ACCESS | WRITE_DAC)
                .unwrap_err();
        assert_eq!(
            error.raw_os_error(),
            Some(i32::try_from(ERROR_ACCESS_DENIED).unwrap())
        );

        drop(pipe);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn single_use_slot_never_exposes_previous_client_bytes() {
        let roots = PrivateRootsV1::discover_for_current_context().unwrap();
        let namespace = roots
            .runtime()
            .endpoint_namespace(unique_project_id())
            .unwrap();
        let cleanup_path = namespace.path().to_path_buf();
        let lease = namespace.acquire_daemon_lease().unwrap();
        let instance = generate_daemon_instance_id().unwrap();
        let mut server = bind(&namespace, instance).unwrap();
        let descriptor =
            EndpointDescriptorV1::for_current_process(namespace.project_id(), instance).unwrap();
        let publication = namespace.publish_endpoint(&lease, descriptor).unwrap();
        let discovered_endpoint = namespace.discover_endpoint().unwrap();
        let initial = discover(&namespace, descriptor).unwrap();

        let (first_server, first_client) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                accept(&mut server),
                discovered_endpoint
                    .connect_verified(&namespace, Instant::now() + Duration::from_secs(5),)
            )
        })
        .await
        .unwrap();
        let (first_server, _) = first_server.unwrap();
        let mut first_client = first_client.unwrap();
        let mut stale_frame = 5_u32.to_be_bytes().to_vec();
        stale_frame.extend_from_slice(b"stale");
        first_client
            .write_frame(
                &stale_frame,
                FrameLimits::bootstrap(),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        tokio::task::yield_now().await;
        drop(first_server);
        let result = first_client
            .read_frame(
                FrameLimits::bootstrap(),
                crate::FrameReadTimeoutsV1::uniform(Duration::from_secs(5)),
            )
            .await;
        assert!(result.is_err() || result.is_ok_and(|frame| frame.is_none()));
        drop(first_client);

        let rotated = discover(&namespace, descriptor).unwrap();
        assert_eq!(rotated.sequence(), initial.sequence() + 1);
        assert_ne!(rotated.slot_id(), initial.slot_id());
        assert_eq!(server.pending.len(), 1);

        let discovered_endpoint = namespace.discover_endpoint().unwrap();
        let (second_server, second_client) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                accept(&mut server),
                discovered_endpoint
                    .connect_verified(&namespace, Instant::now() + Duration::from_secs(5),)
            )
        })
        .await
        .unwrap();
        let (mut second_server, _) = second_server.unwrap();
        let mut second_client = second_client.unwrap();
        let mut fresh_frame = 5_u32.to_be_bytes().to_vec();
        fresh_frame.extend_from_slice(b"fresh");
        second_client
            .write_frame(
                &fresh_frame,
                FrameLimits::bootstrap(),
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        let mut observed = [0_u8; 9];
        tokio::time::timeout(
            Duration::from_secs(5),
            second_server.read_exact(&mut observed),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&observed, fresh_frame.as_slice());
        assert_eq!(server.pending.len(), 1);

        let current = discover(&namespace, descriptor).unwrap();
        let base = pipe_base_name(&namespace, instance);
        let waiting = open_client_pipe(&pipe_slot_name(&base, current.slot_id())).unwrap();
        assert_eq!(server.pending.len(), 1);

        drop(waiting);
        drop(second_server);
        drop(second_client);
        drop(server);
        publication.remove().unwrap();
        drop(lease);
        drop(namespace);
        drop(roots);
        cleanup_namespace(&cleanup_path);
    }
}
