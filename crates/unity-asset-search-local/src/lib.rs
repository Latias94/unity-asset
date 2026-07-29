//! OS-backed project identity and local endpoint discovery.

mod endpoint;
mod ids;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod local_filesystem;
mod project;
mod roots;
mod security_context;
#[cfg(windows)]
mod windows_volume;

pub use endpoint::{
    ENDPOINT_DESCRIPTOR_VERSION, EndpointDescriptorError, EndpointDescriptorV1,
    ExecutableFileIdentityV1, MAX_ENDPOINT_DESCRIPTOR_BYTES, ProcessStartIdentityV1,
};
pub use ids::LocalIdentityParseError;
pub use project::{ProjectIdentityV1, ProjectLocatorError, ProjectLocatorV1};
pub use roots::{PrivateRootKind, PrivateRootV1, PrivateRootsError, PrivateRootsV1};
pub use security_context::{SecurityContextError, SecurityContextIdV1};
