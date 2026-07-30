use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Read as _};

use rand::TryRngCore as _;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use unity_asset_search_protocol::{DaemonInstanceId, ProjectId};

use crate::publication::{self, PublicationSlots, QuarantinedPublication};
use crate::transport::EndpointServerV1;
use crate::{
    EndpointDescriptorError, EndpointDescriptorV1, EndpointNamespaceV1, EndpointTransportError,
    MAX_ENDPOINT_DESCRIPTOR_BYTES, SecurityContextIdV1,
};

const DAEMON_LEASE_FILE: &str = ".daemon-v1.lock";
const ENDPOINT_DESCRIPTOR_FILE: &str = "endpoint.v1.json";
const TEMPORARY_ATTEMPTS: usize = 16;
const PUBLICATION_STAMP_DOMAIN: &[u8] = b"unity-asset:endpoint-publication-stamp:v1\0";
const ENDPOINT_DESCRIPTOR_PUBLICATION: PublicationSlots = PublicationSlots::new(
    ENDPOINT_DESCRIPTOR_FILE,
    ".endpoint-v1.staging",
    Some(".endpoint-v1.quarantine"),
);

impl EndpointNamespaceV1 {
    pub fn claim_daemon_endpoint(&self) -> Result<EndpointClaimV1, EndpointStoreError> {
        let lease = self.acquire_daemon_lease()?;
        let stale_cleanup = self.retire_stale_endpoint(&lease)?;
        Ok(EndpointClaimV1 {
            namespace: self.clone(),
            lease: Some(lease),
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
                security_context_id: self.security_context_id(),
            }),
            Err(source) if is_lock_contention(&source) => Err(EndpointStoreError::LeaseHeld),
            Err(source) => Err(EndpointStoreError::lease_io(source)),
        }
    }

    pub(crate) fn publish_endpoint(
        &self,
        lease: &DaemonLeaseV1,
        descriptor: EndpointDescriptorV1,
    ) -> Result<PublishedEndpointGuardV1, EndpointStoreError> {
        lease.validate_namespace(self)?;
        descriptor.validate_binding(self.project_id(), self.security_context_id())?;
        let encoded = descriptor.encode_json()?;
        let prepared = publication::prepare(self, ENDPOINT_DESCRIPTOR_PUBLICATION, &encoded)
            .map_err(|source| EndpointStoreError::PublicationPreCommit {
                operation: "create endpoint descriptor staging file",
                source,
            })?;
        let commit =
            prepared
                .commit(self)
                .map_err(|source| EndpointStoreError::PublicationPreCommit {
                    operation: "atomically replace endpoint descriptor",
                    source,
                })?;
        let stamp = PublicationStampV1::for_encoded(&encoded);
        let verification_unconfirmed = match self.discover_endpoint() {
            Ok(discovered) => discovered.stamp != stamp || discovered.descriptor != descriptor,
            Err(_) => true,
        };
        Ok(PublishedEndpointGuardV1 {
            namespace: self.clone(),
            descriptor,
            stamp,
            warning: PublicationWarningV1 {
                durability_unconfirmed: commit.durability_unconfirmed(),
                verification_unconfirmed,
            },
            active: true,
        })
    }

    pub fn discover_endpoint(&self) -> Result<DiscoveredEndpointV1, EndpointStoreError> {
        let encoded = read_descriptor(self)?;
        let descriptor = EndpointDescriptorV1::decode_json(&encoded)?;
        descriptor.validate_binding(self.project_id(), self.security_context_id())?;
        Ok(DiscoveredEndpointV1 {
            descriptor,
            stamp: PublicationStampV1::for_encoded(&encoded),
        })
    }

    pub(crate) fn retire_stale_endpoint(
        &self,
        lease: &DaemonLeaseV1,
    ) -> Result<EndpointCleanupV1, EndpointStoreError> {
        lease.validate_namespace(self)?;
        let recovery = publication::recover_abandoned(self, ENDPOINT_DESCRIPTOR_PUBLICATION)
            .map_err(|source| EndpointStoreError::DescriptorIo {
                operation: "recover abandoned endpoint descriptor publication",
                source,
            })?;
        let descriptor = if let Some(quarantine) = claim_descriptor(self)? {
            remove_quarantine(self, quarantine)?;
            EndpointCleanupV1::Removed
        } else {
            EndpointCleanupV1::AlreadyAbsent
        };
        #[cfg(windows)]
        let rendezvous = crate::pipe_rendezvous::retire_stale(self).map_err(|source| {
            EndpointStoreError::DescriptorIo {
                operation: "retire stale Windows pipe rendezvous",
                source,
            }
        })?;
        #[cfg(not(windows))]
        let rendezvous = EndpointCleanupV1::AlreadyAbsent;
        if recovery.removed_any()
            || descriptor == EndpointCleanupV1::Removed
            || rendezvous == EndpointCleanupV1::Removed
        {
            Ok(EndpointCleanupV1::Removed)
        } else {
            Ok(EndpointCleanupV1::AlreadyAbsent)
        }
    }
}

#[must_use = "the endpoint claim must be published or retained to keep the daemon lease"]
pub struct EndpointClaimV1 {
    namespace: EndpointNamespaceV1,
    lease: Option<DaemonLeaseV1>,
    stale_cleanup: EndpointCleanupV1,
}

impl EndpointClaimV1 {
    #[must_use]
    pub const fn stale_cleanup(&self) -> EndpointCleanupV1 {
        self.stale_cleanup
    }

    pub fn publish(
        &mut self,
        daemon_instance_id: DaemonInstanceId,
    ) -> Result<ClaimedEndpointV1, EndpointClaimError> {
        let lease = self
            .lease
            .as_ref()
            .ok_or(EndpointClaimError::AlreadyPublished)?;
        let server = EndpointServerV1::bind_claimed(&self.namespace, lease, daemon_instance_id)?;
        let descriptor = EndpointDescriptorV1::for_current_process(
            self.namespace.project_id(),
            daemon_instance_id,
        )?;
        let publication = self.namespace.publish_endpoint(lease, descriptor)?;
        if publication.warning().verification_unconfirmed() {
            return match publication.remove() {
                Ok(_) => Err(EndpointClaimError::PublicationVerificationUnconfirmed),
                Err(source) => Err(EndpointClaimError::PublicationVerificationCleanup { source }),
            };
        }
        let lease = self
            .lease
            .take()
            .ok_or(EndpointClaimError::AlreadyPublished)?;
        Ok(ClaimedEndpointV1 {
            server: Some(server),
            publication: Some(publication),
            lease,
            stale_cleanup: self.stale_cleanup,
        })
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
    security_context_id: SecurityContextIdV1,
}

impl DaemonLeaseV1 {
    pub(crate) fn validate_namespace(
        &self,
        namespace: &EndpointNamespaceV1,
    ) -> Result<(), EndpointStoreError> {
        if self.project_id != namespace.project_id()
            || self.security_context_id != namespace.security_context_id()
        {
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

#[must_use = "the claimed endpoint must remain alive until listener and task shutdown completes"]
pub struct ClaimedEndpointV1 {
    server: Option<EndpointServerV1>,
    publication: Option<PublishedEndpointGuardV1>,
    lease: DaemonLeaseV1,
    stale_cleanup: EndpointCleanupV1,
}

impl ClaimedEndpointV1 {
    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.lease.project_id
    }

    #[must_use]
    pub fn daemon_instance_id(&self) -> DaemonInstanceId {
        self.server
            .as_ref()
            .expect("a published endpoint retains its server until withdrawal")
            .daemon_instance_id()
    }

    #[must_use]
    pub const fn stale_cleanup(&self) -> EndpointCleanupV1 {
        self.stale_cleanup
    }

    #[must_use]
    pub fn publication_warning(&self) -> PublicationWarningV1 {
        self.publication
            .as_ref()
            .map_or(PublicationWarningV1::default(), |publication| {
                publication.warning()
            })
    }

    pub async fn accept_verified(
        &mut self,
    ) -> Result<crate::VerifiedLocalStreamV1, EndpointTransportError> {
        let server = self
            .server
            .as_mut()
            .ok_or(EndpointTransportError::EndpointUnavailable)?;
        server.accept_verified().await
    }

    pub fn withdraw(&mut self) -> Result<EndpointCleanupV1, EndpointStoreError> {
        let cleanup = match self.publication.take() {
            Some(publication) => publication.remove(),
            None => Ok(EndpointCleanupV1::AlreadyAbsent),
        };
        drop(self.server.take());
        cleanup
    }
}

impl fmt::Debug for ClaimedEndpointV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaimedEndpointV1")
            .field("project_id", &self.project_id())
            .field(
                "daemon_instance_id",
                &self
                    .server
                    .as_ref()
                    .map(|server| server.daemon_instance_id()),
            )
            .field("stale_cleanup", &self.stale_cleanup)
            .field("publication", &self.publication)
            .finish_non_exhaustive()
    }
}

impl Drop for ClaimedEndpointV1 {
    fn drop(&mut self) {
        let _ = self.withdraw();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicationStampV1([u8; 32]);

impl PublicationStampV1 {
    fn for_encoded(encoded: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(PUBLICATION_STAMP_DOMAIN);
        hasher.update(
            u64::try_from(encoded.len())
                .expect("bounded endpoint descriptor length fits u64")
                .to_le_bytes(),
        );
        hasher.update(encoded);
        Self(hasher.finalize().into())
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveredEndpointV1 {
    descriptor: EndpointDescriptorV1,
    stamp: PublicationStampV1,
}

impl DiscoveredEndpointV1 {
    #[must_use]
    pub const fn descriptor(&self) -> EndpointDescriptorV1 {
        self.descriptor
    }

    #[must_use]
    pub const fn publication_stamp(&self) -> PublicationStampV1 {
        self.stamp
    }

    pub fn ensure_unchanged(
        &self,
        namespace: &EndpointNamespaceV1,
    ) -> Result<(), EndpointStoreError> {
        match namespace.discover_endpoint() {
            Ok(current) if current == *self => Ok(()),
            Ok(_) | Err(EndpointStoreError::DescriptorMissing) => {
                Err(EndpointStoreError::EndpointChanged)
            }
            Err(error) => Err(error),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PublicationWarningV1 {
    durability_unconfirmed: bool,
    verification_unconfirmed: bool,
}

impl PublicationWarningV1 {
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
pub enum EndpointCleanupV1 {
    Removed,
    AlreadyAbsent,
    ReplacedPreserved,
}

#[must_use = "the publication guard owns cleanup for the matching endpoint descriptor"]
pub(crate) struct PublishedEndpointGuardV1 {
    namespace: EndpointNamespaceV1,
    descriptor: EndpointDescriptorV1,
    stamp: PublicationStampV1,
    warning: PublicationWarningV1,
    active: bool,
}

impl PublishedEndpointGuardV1 {
    #[must_use]
    pub const fn warning(&self) -> PublicationWarningV1 {
        self.warning
    }

    pub fn remove(mut self) -> Result<EndpointCleanupV1, EndpointStoreError> {
        let outcome = self.remove_if_current()?;
        self.active = false;
        Ok(outcome)
    }

    fn remove_if_current(&self) -> Result<EndpointCleanupV1, EndpointStoreError> {
        let Some(quarantine) = claim_descriptor(&self.namespace)? else {
            return Ok(EndpointCleanupV1::AlreadyAbsent);
        };
        let discovered = match discover_named(&self.namespace, quarantine.name()) {
            Ok(discovered) => discovered,
            Err(error) => {
                restore_quarantine(&self.namespace, quarantine)?;
                return Err(error);
            }
        };
        if discovered.descriptor != self.descriptor || discovered.stamp != self.stamp {
            restore_quarantine(&self.namespace, quarantine)?;
            return Ok(EndpointCleanupV1::ReplacedPreserved);
        }
        remove_quarantine(&self.namespace, quarantine)?;
        Ok(EndpointCleanupV1::Removed)
    }
}

impl fmt::Debug for PublishedEndpointGuardV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublishedEndpointGuardV1")
            .field("descriptor", &self.descriptor)
            .field("stamp", &self.stamp)
            .field("warning", &self.warning)
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

impl Drop for PublishedEndpointGuardV1 {
    fn drop(&mut self) {
        if self.active {
            let _ = self.remove_if_current();
        }
    }
}

#[derive(Debug, Error)]
pub enum EndpointClaimError {
    #[error("endpoint claim has already been published")]
    AlreadyPublished,
    #[error("endpoint descriptor publication committed but read-back verification failed")]
    PublicationVerificationUnconfirmed,
    #[error(
        "endpoint descriptor publication could not be verified and cleanup also failed: {source}"
    )]
    PublicationVerificationCleanup {
        #[source]
        source: EndpointStoreError,
    },
    #[error(transparent)]
    Descriptor(#[from] EndpointDescriptorError),
    #[error(transparent)]
    Store(#[from] EndpointStoreError),
    #[error(transparent)]
    Transport(#[from] EndpointTransportError),
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
    #[error("endpoint descriptor changed while a connection was established")]
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
    Descriptor(#[from] EndpointDescriptorError),
}

impl EndpointStoreError {
    fn lease_io(source: io::Error) -> Self {
        Self::LeaseIo(source)
    }
}

fn read_descriptor(namespace: &EndpointNamespaceV1) -> Result<Vec<u8>, EndpointStoreError> {
    read_descriptor_named(namespace, OsStr::new(ENDPOINT_DESCRIPTOR_FILE))
}

fn discover_named(
    namespace: &EndpointNamespaceV1,
    name: &OsStr,
) -> Result<DiscoveredEndpointV1, EndpointStoreError> {
    let encoded = read_descriptor_named(namespace, name)?;
    let descriptor = EndpointDescriptorV1::decode_json(&encoded)?;
    descriptor.validate_binding(namespace.project_id(), namespace.security_context_id())?;
    Ok(DiscoveredEndpointV1 {
        descriptor,
        stamp: PublicationStampV1::for_encoded(&encoded),
    })
}

fn claim_descriptor(
    namespace: &EndpointNamespaceV1,
) -> Result<Option<QuarantinedPublication>, EndpointStoreError> {
    publication::claim_current(namespace, ENDPOINT_DESCRIPTOR_PUBLICATION).map_err(|source| {
        EndpointStoreError::DescriptorIo {
            operation: "claim endpoint descriptor for cleanup",
            source,
        }
    })
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

fn read_descriptor_named(
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
        .take(u64::try_from(MAX_ENDPOINT_DESCRIPTOR_BYTES).expect("descriptor limit fits u64") + 1)
        .read_to_end(&mut encoded)
        .map_err(|source| EndpointStoreError::DescriptorIo {
            operation: "read endpoint descriptor",
            source,
        })?;
    if encoded.len() > MAX_ENDPOINT_DESCRIPTOR_BYTES {
        return Err(EndpointDescriptorError::EncodedSizeLimit {
            actual: encoded.len(),
            maximum: MAX_ENDPOINT_DESCRIPTOR_BYTES,
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
    fn publication_stamps_bind_exact_bytes() {
        assert_eq!(
            PublicationStampV1::for_encoded(b"one"),
            PublicationStampV1::for_encoded(b"one")
        );
        assert_ne!(
            PublicationStampV1::for_encoded(b"one"),
            PublicationStampV1::for_encoded(b"two")
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn cleanup_guard_never_deletes_a_replacement_publication() {
        let (roots, namespace, cleanup_path) = test_namespace();
        let lease = namespace.acquire_daemon_lease().unwrap();
        let first = EndpointDescriptorV1::for_current_process(
            namespace.project_id(),
            generate_daemon_instance_id().unwrap(),
        )
        .unwrap();
        let second = EndpointDescriptorV1::for_current_process(
            namespace.project_id(),
            generate_daemon_instance_id().unwrap(),
        )
        .unwrap();
        let first_publication = namespace.publish_endpoint(&lease, first).unwrap();
        let second_publication = namespace.publish_endpoint(&lease, second).unwrap();

        assert_eq!(
            first_publication.remove().unwrap(),
            EndpointCleanupV1::ReplacedPreserved
        );
        assert_eq!(namespace.discover_endpoint().unwrap().descriptor(), second);
        assert_eq!(
            second_publication.remove().unwrap(),
            EndpointCleanupV1::Removed
        );

        drop(lease);
        drop(namespace);
        drop(roots);
        cleanup_test_namespace(&cleanup_path);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn endpoint_claim_retires_a_crash_left_descriptor_under_the_lease() {
        let (roots, namespace, cleanup_path) = test_namespace();
        let lease = namespace.acquire_daemon_lease().unwrap();
        let descriptor = EndpointDescriptorV1::for_current_process(
            namespace.project_id(),
            generate_daemon_instance_id().unwrap(),
        )
        .unwrap();
        let mut abandoned = namespace.publish_endpoint(&lease, descriptor).unwrap();
        abandoned.active = false;
        drop(abandoned);
        drop(lease);

        let claim = namespace.claim_daemon_endpoint().unwrap();
        assert_eq!(claim.stale_cleanup(), EndpointCleanupV1::Removed);
        drop(claim);

        drop(namespace);
        drop(roots);
        cleanup_test_namespace(&cleanup_path);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn endpoint_claim_recovers_abandoned_staging_and_quarantine_under_the_lease() {
        let (roots, namespace, cleanup_path) = test_namespace();
        for name in [".endpoint-v1.staging", ".endpoint-v1.quarantine"] {
            let mut file = namespace.create_file(OsStr::new(name)).unwrap();
            file.write_all(b"abandoned endpoint publication").unwrap();
            file.sync_all().unwrap();
        }

        let claim = namespace.claim_daemon_endpoint().unwrap();
        assert_eq!(claim.stale_cleanup(), EndpointCleanupV1::Removed);
        assert!(!cleanup_path.join(".endpoint-v1.staging").exists());
        assert!(!cleanup_path.join(".endpoint-v1.quarantine").exists());
        drop(claim);

        drop(namespace);
        drop(roots);
        cleanup_test_namespace(&cleanup_path);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn endpoint_publication_fails_closed_when_live_staging_exists() {
        let (roots, namespace, cleanup_path) = test_namespace();
        let lease = namespace.acquire_daemon_lease().unwrap();
        namespace
            .create_file(OsStr::new(".endpoint-v1.staging"))
            .unwrap();
        let descriptor = EndpointDescriptorV1::for_current_process(
            namespace.project_id(),
            generate_daemon_instance_id().unwrap(),
        )
        .unwrap();

        assert!(matches!(
            namespace.publish_endpoint(&lease, descriptor),
            Err(EndpointStoreError::PublicationPreCommit { .. })
        ));
        namespace
            .remove_file(OsStr::new(".endpoint-v1.staging"))
            .unwrap();

        drop(lease);
        drop(namespace);
        drop(roots);
        cleanup_test_namespace(&cleanup_path);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn restoring_an_old_quarantine_preserves_a_newer_descriptor_and_reclaims_the_old_file() {
        let (roots, namespace, cleanup_path) = test_namespace();
        let lease = namespace.acquire_daemon_lease().unwrap();
        let first = EndpointDescriptorV1::for_current_process(
            namespace.project_id(),
            generate_daemon_instance_id().unwrap(),
        )
        .unwrap();
        let second = EndpointDescriptorV1::for_current_process(
            namespace.project_id(),
            generate_daemon_instance_id().unwrap(),
        )
        .unwrap();
        let mut first_publication = namespace.publish_endpoint(&lease, first).unwrap();
        first_publication.active = false;
        let quarantine = claim_descriptor(&namespace).unwrap().unwrap();
        let second_publication = namespace.publish_endpoint(&lease, second).unwrap();

        restore_quarantine(&namespace, quarantine).unwrap();
        assert_eq!(namespace.discover_endpoint().unwrap().descriptor(), second);
        assert!(!cleanup_path.join(".endpoint-v1.quarantine").exists());
        assert_eq!(
            second_publication.remove().unwrap(),
            EndpointCleanupV1::Removed
        );

        drop(first_publication);
        drop(lease);
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
