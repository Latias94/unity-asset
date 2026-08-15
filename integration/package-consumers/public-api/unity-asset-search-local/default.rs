//! Public API contract for the `unity-asset-search-local` package.

pub use unity_asset_search_local::{
    DiscoveredLoopbackEndpoint, EndpointNamespaceV1, EndpointStoreError, HttpCapability,
    LoopbackEndpointClaim, LoopbackEndpointCleanup, LoopbackEndpointDescriptor,
    LoopbackEndpointPublishError, PrivateIndexRootV1, PrivateRootsV1, ProjectIdentityV1,
    ProjectLocatorV1, PublishedLoopbackEndpoint, generate_daemon_instance_id,
};
