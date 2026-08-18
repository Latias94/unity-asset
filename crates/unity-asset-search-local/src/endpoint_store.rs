use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Read as _};

use rand::TryRngCore as _;
use thiserror::Error;
use unity_asset_search_protocol::{DaemonInstanceId, ProjectId, QueryPolicyId};

use crate::publication::{self, PublicationSlots, QuarantinedPublication};
use crate::{
    EndpointNamespaceV1, HttpCapability, HttpCapabilityError, LoopbackEndpointDescriptor,
    LoopbackEndpointDescriptorError, MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES,
};

const DAEMON_LEASE_FILE: &str = ".daemon-v1.lock";
const LOOPBACK_ENDPOINT_DESCRIPTOR_FILE: &str = "endpoint.v2.json";
const TEMPORARY_ATTEMPTS: usize = 16;
#[cfg(windows)]
const WINDOWS_DESCRIPTOR_CLAIM_RETRY_WINDOW: std::time::Duration =
    std::time::Duration::from_millis(500);
#[cfg(windows)]
const WINDOWS_DESCRIPTOR_CLAIM_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_millis(10);
const LOOPBACK_ENDPOINT_DESCRIPTOR_PUBLICATION: PublicationSlots = PublicationSlots::new(
    LOOPBACK_ENDPOINT_DESCRIPTOR_FILE,
    ".endpoint-v2.staging",
    Some(".endpoint-v2.quarantine"),
);

impl EndpointNamespaceV1 {
    /// Acquires the single-daemon lease and creates this process instance's HTTP capability.
    ///
    /// The claim does not bind or own a listener. A daemon can bind `127.0.0.1:0`, configure its
    /// HTTP service with [`LoopbackEndpointClaim::capability`], and publish the selected port only
    /// after the service is ready.
    pub fn claim_loopback_endpoint(&self) -> Result<LoopbackEndpointClaim, EndpointStoreError> {
        let lease = self.acquire_daemon_lease()?;
        let stale_cleanup = self.retire_stale_loopback_endpoint(&lease)?;
        let capability = HttpCapability::generate()?;
        Ok(LoopbackEndpointClaim {
            namespace: self.clone(),
            lease: Some(lease),
            capability,
            stale_cleanup,
        })
    }

    pub(crate) fn acquire_daemon_lease(&self) -> Result<DaemonLeaseV1, EndpointStoreError> {
        let file = match self.create_file(OsStr::new(DAEMON_LEASE_FILE)) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => self
                .open_file(OsStr::new(DAEMON_LEASE_FILE), true)
                .map_err(EndpointStoreError::lease_io)?,
            Err(source) => return Err(EndpointStoreError::lease_io(source)),
        };
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(DaemonLeaseV1 {
                file,
                project_id: self.project_id(),
            }),
            Err(source) if is_lock_contention(&source) => Err(EndpointStoreError::LeaseHeld),
            Err(source) => Err(EndpointStoreError::lease_io(source)),
        }
    }

    pub(crate) fn publish_loopback_endpoint(
        &self,
        lease: &DaemonLeaseV1,
        descriptor: &LoopbackEndpointDescriptor,
    ) -> Result<PublishedLoopbackEndpointGuard, EndpointStoreError> {
        lease.validate_namespace(self)?;
        descriptor.validate_project(self.project_id())?;
        let encoded = descriptor.encode_json()?;
        let prepared =
            publication::prepare(self, LOOPBACK_ENDPOINT_DESCRIPTOR_PUBLICATION, &encoded)
                .map_err(|source| EndpointStoreError::PublicationPreCommit {
                    operation: "create loopback endpoint descriptor staging file",
                    source,
                })?;
        let commit =
            prepared
                .commit(self)
                .map_err(|source| EndpointStoreError::PublicationPreCommit {
                    operation: "atomically replace loopback endpoint descriptor",
                    source,
                })?;
        let verification_unconfirmed = match read_loopback_descriptor(self) {
            Ok(published) => published != encoded,
            Err(_) => true,
        };
        Ok(PublishedLoopbackEndpointGuard {
            namespace: self.clone(),
            generation: LoopbackEndpointGeneration::new(encoded),
            cleanup_retry: LoopbackDescriptorCleanupRetry::default(),
            warning: LoopbackEndpointPublicationWarning {
                durability_unconfirmed: commit.durability_unconfirmed(),
                verification_unconfirmed,
            },
            active: true,
        })
    }

    /// Discovers the capability-bound V2 descriptor.
    pub fn discover_loopback_endpoint(
        &self,
    ) -> Result<DiscoveredLoopbackEndpoint, EndpointStoreError> {
        discover_loopback_named(self, OsStr::new(LOOPBACK_ENDPOINT_DESCRIPTOR_FILE))
    }

    fn retire_stale_loopback_endpoint(
        &self,
        lease: &DaemonLeaseV1,
    ) -> Result<LoopbackEndpointCleanup, EndpointStoreError> {
        lease.validate_namespace(self)?;
        let recovery =
            publication::recover_abandoned(self, LOOPBACK_ENDPOINT_DESCRIPTOR_PUBLICATION)
                .map_err(|source| EndpointStoreError::DescriptorIo {
                    operation: "recover abandoned loopback endpoint descriptor publication",
                    source,
                })?;
        let mut cleanup_retry = LoopbackDescriptorCleanupRetry::default();
        let descriptor =
            if let Some(quarantine) = claim_loopback_descriptor(self, &mut cleanup_retry)? {
                remove_quarantine(self, quarantine)?;
                LoopbackEndpointCleanup::Removed
            } else {
                LoopbackEndpointCleanup::AlreadyAbsent
            };
        if recovery.removed_any() || descriptor == LoopbackEndpointCleanup::Removed {
            Ok(LoopbackEndpointCleanup::Removed)
        } else {
            Ok(LoopbackEndpointCleanup::AlreadyAbsent)
        }
    }
}

/// Lease-backed authority to publish one already-bound loopback HTTP endpoint.
#[must_use = "the loopback endpoint claim must be published or retained to keep the daemon lease"]
pub struct LoopbackEndpointClaim {
    namespace: EndpointNamespaceV1,
    lease: Option<DaemonLeaseV1>,
    capability: HttpCapability,
    stale_cleanup: LoopbackEndpointCleanup,
}

impl LoopbackEndpointClaim {
    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.namespace.project_id()
    }

    /// Returns the process-instance credential so the HTTP service can authenticate every route.
    #[must_use]
    pub const fn capability(&self) -> &HttpCapability {
        &self.capability
    }

    #[must_use]
    pub const fn stale_cleanup(&self) -> LoopbackEndpointCleanup {
        self.stale_cleanup
    }

    /// Publishes the operating-system-selected port after the separately owned listener is ready.
    ///
    /// A failed publication retains the daemon lease so the caller can stop any started work and
    /// either retry publication or drop the claim after cleanup completes.
    pub fn publish(
        &mut self,
        daemon_instance_id: DaemonInstanceId,
        port: u16,
        query_policy_id: QueryPolicyId,
    ) -> Result<PublishedLoopbackEndpoint, LoopbackEndpointPublishError> {
        let lease = self
            .lease
            .as_ref()
            .ok_or(LoopbackEndpointPublishError::AlreadyPublished)?;
        let descriptor = LoopbackEndpointDescriptor::for_current_process(
            self.namespace.project_id(),
            daemon_instance_id,
            port,
            self.capability.clone(),
            query_policy_id,
        )?;
        let mut publication = self
            .namespace
            .publish_loopback_endpoint(lease, &descriptor)?;
        if publication.warning().verification_unconfirmed() {
            return match publication.remove() {
                Ok(_) => Err(LoopbackEndpointPublishError::PublicationVerificationUnconfirmed),
                Err(source) => {
                    Err(LoopbackEndpointPublishError::PublicationVerificationCleanup { source })
                }
            };
        }
        let lease = self
            .lease
            .take()
            .ok_or(LoopbackEndpointPublishError::AlreadyPublished)?;
        Ok(PublishedLoopbackEndpoint {
            descriptor,
            publication: Some(publication),
            lease,
            stale_cleanup: self.stale_cleanup,
        })
    }
}

impl fmt::Debug for LoopbackEndpointClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoopbackEndpointClaim")
            .field("project_id", &self.project_id())
            .field("capability", &self.capability)
            .field("stale_cleanup", &self.stale_cleanup)
            .finish_non_exhaustive()
    }
}

/// Published discovery ownership independent of the HTTP listener and serving task.
#[must_use = "the published endpoint must remain alive until discovery is withdrawn"]
pub struct PublishedLoopbackEndpoint {
    descriptor: LoopbackEndpointDescriptor,
    publication: Option<PublishedLoopbackEndpointGuard>,
    lease: DaemonLeaseV1,
    stale_cleanup: LoopbackEndpointCleanup,
}

impl PublishedLoopbackEndpoint {
    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.lease.project_id
    }

    #[must_use]
    pub const fn descriptor(&self) -> &LoopbackEndpointDescriptor {
        &self.descriptor
    }

    #[must_use]
    pub const fn stale_cleanup(&self) -> LoopbackEndpointCleanup {
        self.stale_cleanup
    }

    #[must_use]
    pub fn publication_warning(&self) -> LoopbackEndpointPublicationWarning {
        self.publication.as_ref().map_or(
            LoopbackEndpointPublicationWarning::default(),
            PublishedLoopbackEndpointGuard::warning,
        )
    }

    /// Withdraws discovery before the independently owned HTTP service begins shutdown draining.
    pub fn withdraw(&mut self) -> Result<LoopbackEndpointCleanup, EndpointStoreError> {
        let Some(publication) = self.publication.as_mut() else {
            return Ok(LoopbackEndpointCleanup::AlreadyAbsent);
        };
        let cleanup = publication.remove()?;
        self.publication = None;
        Ok(cleanup)
    }
}

impl fmt::Debug for PublishedLoopbackEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublishedLoopbackEndpoint")
            .field("project_id", &self.project_id())
            .field("descriptor", &self.descriptor)
            .field("stale_cleanup", &self.stale_cleanup)
            .field("publication", &self.publication)
            .finish_non_exhaustive()
    }
}

impl Drop for PublishedLoopbackEndpoint {
    fn drop(&mut self) {
        let _ = self.withdraw();
    }
}

pub fn generate_daemon_instance_id() -> Result<DaemonInstanceId, EndpointStoreError> {
    let mut random = rand::rngs::OsRng;
    for _ in 0..TEMPORARY_ATTEMPTS {
        let mut bytes = [0_u8; 16];
        random.try_fill_bytes(&mut bytes).map_err(|source| {
            EndpointStoreError::EntropyUnavailable {
                message: source.to_string(),
            }
        })?;
        if bytes.iter().any(|byte| *byte != 0) {
            return Ok(DaemonInstanceId::from_bytes(bytes));
        }
    }
    Err(EndpointStoreError::EntropyReturnedOnlyZeroValues)
}

#[derive(Debug)]
#[must_use = "the daemon lease must remain alive until endpoint cleanup completes"]
pub struct DaemonLeaseV1 {
    file: File,
    project_id: ProjectId,
}

impl DaemonLeaseV1 {
    pub(crate) fn validate_namespace(
        &self,
        namespace: &EndpointNamespaceV1,
    ) -> Result<(), EndpointStoreError> {
        if self.project_id != namespace.project_id() {
            return Err(EndpointStoreError::LeaseNamespaceMismatch);
        }
        Ok(())
    }
}

impl Drop for DaemonLeaseV1 {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

#[derive(Clone, PartialEq, Eq)]
struct LoopbackEndpointGeneration(Box<[u8]>);

impl LoopbackEndpointGeneration {
    fn new(encoded: Vec<u8>) -> Self {
        Self(encoded.into_boxed_slice())
    }

    fn matches(&self, encoded: &[u8]) -> bool {
        self.0.as_ref() == encoded
    }
}

impl fmt::Debug for LoopbackEndpointGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoopbackEndpointGeneration")
            .field("encoded_bytes", &self.0.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredLoopbackEndpoint {
    descriptor: LoopbackEndpointDescriptor,
    generation: LoopbackEndpointGeneration,
}

impl DiscoveredLoopbackEndpoint {
    #[must_use]
    pub const fn descriptor(&self) -> &LoopbackEndpointDescriptor {
        &self.descriptor
    }

    pub fn ensure_unchanged(
        &self,
        namespace: &EndpointNamespaceV1,
    ) -> Result<(), EndpointStoreError> {
        match read_loopback_descriptor(namespace) {
            Ok(encoded) if self.generation.matches(&encoded) => {
                self.descriptor.validate_project(namespace.project_id())?;
                Ok(())
            }
            Ok(_)
            | Err(EndpointStoreError::DescriptorMissing)
            | Err(EndpointStoreError::LoopbackDescriptor(
                LoopbackEndpointDescriptorError::EncodedSizeLimit { .. },
            )) => Err(EndpointStoreError::EndpointChanged),
            Err(error) => Err(error),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoopbackEndpointPublicationWarning {
    durability_unconfirmed: bool,
    verification_unconfirmed: bool,
}

impl LoopbackEndpointPublicationWarning {
    #[must_use]
    pub const fn durability_unconfirmed(&self) -> bool {
        self.durability_unconfirmed
    }

    #[must_use]
    pub const fn verification_unconfirmed(&self) -> bool {
        self.verification_unconfirmed
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.durability_unconfirmed && !self.verification_unconfirmed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopbackEndpointCleanup {
    Removed,
    AlreadyAbsent,
    ReplacedPreserved,
}

#[must_use = "the publication guard owns cleanup for the matching loopback descriptor"]
pub(crate) struct PublishedLoopbackEndpointGuard {
    namespace: EndpointNamespaceV1,
    generation: LoopbackEndpointGeneration,
    cleanup_retry: LoopbackDescriptorCleanupRetry,
    warning: LoopbackEndpointPublicationWarning,
    active: bool,
}

impl PublishedLoopbackEndpointGuard {
    #[must_use]
    pub const fn warning(&self) -> LoopbackEndpointPublicationWarning {
        self.warning
    }

    pub fn remove(&mut self) -> Result<LoopbackEndpointCleanup, EndpointStoreError> {
        let outcome = self.remove_if_current()?;
        self.active = false;
        Ok(outcome)
    }

    fn remove_if_current(&mut self) -> Result<LoopbackEndpointCleanup, EndpointStoreError> {
        let Some(quarantine) = claim_loopback_descriptor(&self.namespace, &mut self.cleanup_retry)?
        else {
            return Ok(LoopbackEndpointCleanup::AlreadyAbsent);
        };
        let encoded = match read_loopback_descriptor_named(&self.namespace, quarantine.name()) {
            Ok(encoded) => encoded,
            Err(EndpointStoreError::LoopbackDescriptor(
                LoopbackEndpointDescriptorError::EncodedSizeLimit { .. },
            )) => {
                restore_quarantine(&self.namespace, quarantine)?;
                return Ok(LoopbackEndpointCleanup::ReplacedPreserved);
            }
            Err(error) => {
                restore_quarantine(&self.namespace, quarantine)?;
                return Err(error);
            }
        };
        if !self.generation.matches(&encoded) {
            restore_quarantine(&self.namespace, quarantine)?;
            return Ok(LoopbackEndpointCleanup::ReplacedPreserved);
        }
        remove_quarantine(&self.namespace, quarantine)?;
        Ok(LoopbackEndpointCleanup::Removed)
    }
}

impl fmt::Debug for PublishedLoopbackEndpointGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublishedLoopbackEndpointGuard")
            .field("generation", &self.generation)
            .field("warning", &self.warning)
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

impl Drop for PublishedLoopbackEndpointGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = self.remove_if_current();
        }
    }
}

#[derive(Debug, Error)]
pub enum LoopbackEndpointPublishError {
    #[error("loopback endpoint claim has already been published")]
    AlreadyPublished,
    #[error("loopback endpoint descriptor publication committed but read-back verification failed")]
    PublicationVerificationUnconfirmed,
    #[error(
        "loopback endpoint descriptor publication could not be verified and cleanup also failed: {source}"
    )]
    PublicationVerificationCleanup {
        #[source]
        source: EndpointStoreError,
    },
    #[error(transparent)]
    Descriptor(#[from] LoopbackEndpointDescriptorError),
    #[error(transparent)]
    Store(#[from] EndpointStoreError),
}

#[derive(Debug, Error)]
pub enum EndpointStoreError {
    #[error("another daemon already owns this project endpoint lease")]
    LeaseHeld,
    #[error("daemon lease belongs to another project endpoint namespace")]
    LeaseNamespaceMismatch,
    #[error("could not access the project endpoint lease: {0}")]
    LeaseIo(#[source] io::Error),
    #[error("endpoint descriptor is missing")]
    DescriptorMissing,
    #[error("could not {operation}: {source}")]
    DescriptorIo {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("endpoint descriptor changed while a request was in flight")]
    EndpointChanged,
    #[error(
        "endpoint publication failed before its commit point while trying to {operation}: {source}"
    )]
    PublicationPreCommit {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("could not obtain operating-system entropy: {message}")]
    EntropyUnavailable { message: String },
    #[error("operating-system entropy repeatedly returned the reserved zero identifier")]
    EntropyReturnedOnlyZeroValues,
    #[error(transparent)]
    Capability(#[from] HttpCapabilityError),
    #[error(transparent)]
    LoopbackDescriptor(#[from] LoopbackEndpointDescriptorError),
}

impl EndpointStoreError {
    fn lease_io(source: io::Error) -> Self {
        Self::LeaseIo(source)
    }
}

fn read_loopback_descriptor(
    namespace: &EndpointNamespaceV1,
) -> Result<Vec<u8>, EndpointStoreError> {
    read_loopback_descriptor_named(namespace, OsStr::new(LOOPBACK_ENDPOINT_DESCRIPTOR_FILE))
}

fn discover_loopback_named(
    namespace: &EndpointNamespaceV1,
    name: &OsStr,
) -> Result<DiscoveredLoopbackEndpoint, EndpointStoreError> {
    let encoded = read_loopback_descriptor_named(namespace, name)?;
    let descriptor = LoopbackEndpointDescriptor::decode_json(&encoded)?;
    descriptor.validate_project(namespace.project_id())?;
    Ok(DiscoveredLoopbackEndpoint {
        descriptor,
        generation: LoopbackEndpointGeneration::new(encoded),
    })
}

fn claim_loopback_descriptor(
    namespace: &EndpointNamespaceV1,
    cleanup_retry: &mut LoopbackDescriptorCleanupRetry,
) -> Result<Option<QuarantinedPublication>, EndpointStoreError> {
    cleanup_retry
        .claim(namespace)
        .map_err(|source| EndpointStoreError::DescriptorIo {
            operation: "claim loopback endpoint descriptor for cleanup",
            source,
        })
}

#[derive(Default)]
struct LoopbackDescriptorCleanupRetry {
    #[cfg(windows)]
    deadline: Option<std::time::Instant>,
}

impl LoopbackDescriptorCleanupRetry {
    fn claim(
        &mut self,
        namespace: &EndpointNamespaceV1,
    ) -> io::Result<Option<QuarantinedPublication>> {
        #[cfg(not(windows))]
        {
            publication::claim_current(namespace, LOOPBACK_ENDPOINT_DESCRIPTOR_PUBLICATION)
        }

        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};

            let deadline = *self.deadline.get_or_insert_with(|| {
                std::time::Instant::now() + WINDOWS_DESCRIPTOR_CLAIM_RETRY_WINDOW
            });
            loop {
                match publication::claim_current(
                    namespace,
                    LOOPBACK_ENDPOINT_DESCRIPTOR_PUBLICATION,
                ) {
                    Err(source)
                        if matches!(
                            source.raw_os_error(),
                            Some(code)
                                if code == ERROR_SHARING_VIOLATION as i32
                                    || code == ERROR_LOCK_VIOLATION as i32
                        ) =>
                    {
                        let remaining =
                            deadline.saturating_duration_since(std::time::Instant::now());
                        if remaining.is_zero() {
                            return Err(source);
                        }
                        // Cross-language readers cannot universally opt into FILE_SHARE_DELETE.
                        // Preserve the atomic quarantine protocol while a short read completes.
                        std::thread::sleep(WINDOWS_DESCRIPTOR_CLAIM_RETRY_DELAY.min(remaining));
                    }
                    result => return result,
                }
            }
        }
    }
}

fn restore_quarantine(
    namespace: &EndpointNamespaceV1,
    quarantine: QuarantinedPublication,
) -> Result<(), EndpointStoreError> {
    match quarantine.restore(namespace) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(EndpointStoreError::DescriptorIo {
            operation: "restore a non-owned endpoint descriptor from quarantine",
            source,
        }),
    }
}

fn remove_quarantine(
    namespace: &EndpointNamespaceV1,
    quarantine: QuarantinedPublication,
) -> Result<(), EndpointStoreError> {
    match quarantine.remove(namespace) {
        Ok(()) => Ok(()),
        Err(source) => Err(EndpointStoreError::DescriptorIo {
            operation: "remove claimed endpoint descriptor",
            source,
        }),
    }
}

fn read_loopback_descriptor_named(
    namespace: &EndpointNamespaceV1,
    name: &OsStr,
) -> Result<Vec<u8>, EndpointStoreError> {
    let mut file = namespace.open_file(name, false).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            EndpointStoreError::DescriptorMissing
        } else {
            EndpointStoreError::DescriptorIo {
                operation: "open endpoint descriptor",
                source,
            }
        }
    })?;
    let mut encoded = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(
            u64::try_from(MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES)
                .expect("descriptor limit fits u64")
                + 1,
        )
        .read_to_end(&mut encoded)
        .map_err(|source| EndpointStoreError::DescriptorIo {
            operation: "read endpoint descriptor",
            source,
        })?;
    if encoded.len() > MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES {
        return Err(LoopbackEndpointDescriptorError::EncodedSizeLimit {
            actual: encoded.len(),
            maximum: MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES,
        }
        .into());
    }
    Ok(encoded)
}

fn is_lock_contention(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock || cfg!(windows) && error.raw_os_error() == Some(33)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    use crate::PrivateRootsV1;

    #[test]
    fn loopback_endpoint_generations_bind_exact_bytes() {
        assert_eq!(
            LoopbackEndpointGeneration::new(b"one".to_vec()),
            LoopbackEndpointGeneration::new(b"one".to_vec())
        );
        assert_ne!(
            LoopbackEndpointGeneration::new(b"one".to_vec()),
            LoopbackEndpointGeneration::new(b"two".to_vec())
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn stale_capability_is_detected_and_old_cleanup_preserves_the_replacement() {
        let (roots, namespace, cleanup_path) = test_namespace();
        let lease = namespace.acquire_daemon_lease().unwrap();
        let daemon_instance_id = generate_daemon_instance_id().unwrap();
        let query_policy_id = unity_asset_search_protocol::QueryPolicyId::from_bytes([0x44; 32]);
        let first = LoopbackEndpointDescriptor::for_current_process(
            namespace.project_id(),
            daemon_instance_id,
            42_424,
            HttpCapability::from_bytes([0x11; 32]).unwrap(),
            query_policy_id,
        )
        .unwrap();
        let second = LoopbackEndpointDescriptor::for_current_process(
            namespace.project_id(),
            daemon_instance_id,
            42_424,
            HttpCapability::from_bytes([0x22; 32]).unwrap(),
            query_policy_id,
        )
        .unwrap();
        let mut first_publication = namespace.publish_loopback_endpoint(&lease, &first).unwrap();
        let first_discovery = namespace.discover_loopback_endpoint().unwrap();
        let mut second_publication = namespace
            .publish_loopback_endpoint(&lease, &second)
            .unwrap();

        assert!(matches!(
            first_discovery.ensure_unchanged(&namespace),
            Err(EndpointStoreError::EndpointChanged)
        ));
        assert_eq!(
            first_publication.remove().unwrap(),
            LoopbackEndpointCleanup::ReplacedPreserved
        );
        assert_eq!(
            namespace.discover_loopback_endpoint().unwrap().descriptor(),
            &second
        );
        assert_eq!(
            second_publication.remove().unwrap(),
            LoopbackEndpointCleanup::Removed
        );

        drop(lease);
        drop(namespace);
        drop(roots);
        cleanup_test_namespace(&cleanup_path);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn generation_check_compares_publication_bytes_without_reparsing_replacements() {
        let (roots, namespace, cleanup_path) = test_namespace();
        let lease = namespace.acquire_daemon_lease().unwrap();
        let descriptor = LoopbackEndpointDescriptor::for_current_process(
            namespace.project_id(),
            generate_daemon_instance_id().unwrap(),
            42_424,
            HttpCapability::from_bytes([0x11; 32]).unwrap(),
            QueryPolicyId::from_bytes([0x44; 32]),
        )
        .unwrap();
        let canonical = descriptor.encode_json().unwrap();
        let mut publication = namespace
            .publish_loopback_endpoint(&lease, &descriptor)
            .unwrap();
        let discovered = namespace.discover_loopback_endpoint().unwrap();
        let descriptor_path = cleanup_path.join(LOOPBACK_ENDPOINT_DESCRIPTOR_FILE);

        std::fs::write(&descriptor_path, b"{").unwrap();
        assert!(matches!(
            discovered.ensure_unchanged(&namespace),
            Err(EndpointStoreError::EndpointChanged)
        ));

        let mut noncanonical = canonical.clone();
        noncanonical.push(b'\n');
        std::fs::write(&descriptor_path, noncanonical).unwrap();
        assert!(matches!(
            discovered.ensure_unchanged(&namespace),
            Err(EndpointStoreError::EndpointChanged)
        ));

        std::fs::write(&descriptor_path, canonical).unwrap();
        discovered.ensure_unchanged(&namespace).unwrap();
        assert_eq!(
            publication.remove().unwrap(),
            LoopbackEndpointCleanup::Removed
        );

        drop(lease);
        drop(namespace);
        drop(roots);
        cleanup_test_namespace(&cleanup_path);
    }

    #[cfg(windows)]
    #[test]
    fn cleanup_waits_for_a_short_lived_descriptor_reader() {
        use std::os::windows::fs::OpenOptionsExt as _;
        use std::time::Duration;

        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let (roots, namespace, cleanup_path) = test_namespace();
        let lease = namespace.acquire_daemon_lease().unwrap();
        let descriptor = LoopbackEndpointDescriptor::for_current_process(
            namespace.project_id(),
            generate_daemon_instance_id().unwrap(),
            42_424,
            HttpCapability::from_bytes([0x11; 32]).unwrap(),
            QueryPolicyId::from_bytes([0x44; 32]),
        )
        .unwrap();
        let mut publication = namespace
            .publish_loopback_endpoint(&lease, &descriptor)
            .unwrap();
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(cleanup_path.join(LOOPBACK_ENDPOINT_DESCRIPTOR_FILE))
            .unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(reader);
        });

        let cleanup = publication.remove();
        releaser.join().unwrap();
        drop(publication);
        drop(lease);
        drop(namespace);
        drop(roots);
        cleanup_test_namespace(&cleanup_path);

        assert_eq!(cleanup.unwrap(), LoopbackEndpointCleanup::Removed);
    }

    #[cfg(windows)]
    #[test]
    fn cleanup_drop_does_not_restart_an_exhausted_contention_window() {
        use std::os::windows::fs::OpenOptionsExt as _;
        use std::time::Duration;

        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let (roots, namespace, cleanup_path) = test_namespace();
        let lease = namespace.acquire_daemon_lease().unwrap();
        let descriptor = LoopbackEndpointDescriptor::for_current_process(
            namespace.project_id(),
            generate_daemon_instance_id().unwrap(),
            42_424,
            HttpCapability::from_bytes([0x11; 32]).unwrap(),
            QueryPolicyId::from_bytes([0x44; 32]),
        )
        .unwrap();
        let mut publication = namespace
            .publish_loopback_endpoint(&lease, &descriptor)
            .unwrap();
        let descriptor_path = cleanup_path.join(LOOPBACK_ENDPOINT_DESCRIPTOR_FILE);
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&descriptor_path)
            .unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(750));
            drop(reader);
        });

        assert!(matches!(
            publication.remove(),
            Err(EndpointStoreError::DescriptorIo { source, .. })
                if source.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
        ));
        drop(publication);
        releaser.join().unwrap();
        assert!(descriptor_path.exists());

        let mut cleanup_retry = LoopbackDescriptorCleanupRetry::default();
        let quarantine = claim_loopback_descriptor(&namespace, &mut cleanup_retry)
            .unwrap()
            .unwrap();
        remove_quarantine(&namespace, quarantine).unwrap();
        drop(lease);
        drop(namespace);
        drop(roots);
        cleanup_test_namespace(&cleanup_path);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn loopback_claim_recovers_abandoned_v2_publication_files_under_the_lease() {
        let (roots, namespace, cleanup_path) = test_namespace();
        for name in [".endpoint-v2.staging", ".endpoint-v2.quarantine"] {
            let mut file = namespace.create_file(OsStr::new(name)).unwrap();
            file.write_all(b"abandoned loopback endpoint publication")
                .unwrap();
            file.sync_all().unwrap();
        }

        let claim = namespace.claim_loopback_endpoint().unwrap();
        assert_eq!(claim.stale_cleanup(), LoopbackEndpointCleanup::Removed);
        assert!(!cleanup_path.join(".endpoint-v2.staging").exists());
        assert!(!cleanup_path.join(".endpoint-v2.quarantine").exists());
        drop(claim);

        drop(namespace);
        drop(roots);
        cleanup_test_namespace(&cleanup_path);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    fn test_namespace() -> (PrivateRootsV1, EndpointNamespaceV1, std::path::PathBuf) {
        let roots = PrivateRootsV1::discover_for_current_context().unwrap();
        let mut project_bytes = rand::random::<[u8; 32]>();
        project_bytes[0] |= 1;
        let namespace = roots
            .runtime()
            .endpoint_namespace(ProjectId::from_bytes(project_bytes))
            .unwrap();
        let cleanup_path = namespace.path().to_path_buf();
        (roots, namespace, cleanup_path)
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    fn cleanup_test_namespace(path: &std::path::Path) {
        for name in [DAEMON_LEASE_FILE, "binding.v1", ".binding-v1.lock"] {
            let result = std::fs::remove_file(path.join(name));
            assert!(
                result.is_ok()
                    || result.is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
            );
        }
        std::fs::remove_dir(path).unwrap();
    }
}
