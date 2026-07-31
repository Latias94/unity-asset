//! OS-backed project identity and local endpoint discovery.

mod endpoint;
mod endpoint_store;
mod ids;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod local_filesystem;
#[cfg(windows)]
mod pipe_rendezvous;
mod process;
mod project;
mod publication;
mod roots;
mod security_context;
mod transport;
#[cfg(windows)]
mod windows_volume;

pub use endpoint::{
    ENDPOINT_DESCRIPTOR_VERSION, EndpointDescriptorError, EndpointDescriptorV1,
    MAX_ENDPOINT_DESCRIPTOR_BYTES, ProcessStartIdentityV1,
};
pub use endpoint_store::{
    ClaimedEndpointV1, DiscoveredEndpointV1, EndpointClaimError, EndpointClaimV1,
    EndpointCleanupV1, EndpointStoreError, PublicationStampV1, PublicationWarningV1,
    generate_daemon_instance_id,
};
pub use ids::LocalIdentityParseError;
pub use process::{ProcessIdentityError, ProcessIdentityV1};
pub use project::{ProjectIdentityV1, ProjectLocatorError, ProjectLocatorV1};
pub use roots::{
    EndpointNamespaceV1, PrivateIndexRootV1, PrivateRootKind, PrivateRootV1, PrivateRootsError,
    PrivateRootsV1,
};
pub use security_context::{SecurityContextError, SecurityContextIdV1};
pub use transport::{EndpointTransportError, MAX_LOCAL_IPC_CONNECTIONS_V1, VerifiedLocalStreamV1};
