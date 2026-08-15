//! OS-backed project identity and local endpoint discovery.

mod endpoint;
mod endpoint_store;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod local_filesystem;
mod project;
mod publication;
mod roots;
mod security_context;
#[cfg(windows)]
mod windows_volume;

pub use endpoint::{
    HTTP_CAPABILITY_BYTES, HTTP_CAPABILITY_HEX_BYTES, HttpCapability, HttpCapabilityError,
    LOOPBACK_ENDPOINT_DESCRIPTOR_VERSION, LOOPBACK_HTTP_REQUEST_PATH, LoopbackEndpointDescriptor,
    LoopbackEndpointDescriptorError, MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES,
};
pub use endpoint_store::{
    DiscoveredLoopbackEndpoint, EndpointStoreError, LoopbackEndpointClaim, LoopbackEndpointCleanup,
    LoopbackEndpointPublicationWarning, LoopbackEndpointPublishError, PublishedLoopbackEndpoint,
    generate_daemon_instance_id,
};
pub use project::{ProjectIdentityV1, ProjectLocatorError, ProjectLocatorV1};
pub use roots::{
    EndpointNamespaceV1, PrivateIndexRootV1, PrivateRootKind, PrivateRootV1, PrivateRootsError,
    PrivateRootsV1,
};
pub use security_context::FilesystemAuthorityError;
