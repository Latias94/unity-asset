use std::ffi::OsStr;
use std::io::{self, Read as _};

use rand::TryRngCore as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use unity_asset_search_protocol::{DaemonInstanceId, ProjectId};

use crate::publication::{self, PublicationSlots, QuarantinedPublication};
use crate::transport::EndpointTransportError;
use crate::{EndpointCleanupV1, EndpointDescriptorV1, EndpointNamespaceV1, SecurityContextIdV1};

const RENDEZVOUS_FILE: &str = "windows-pipe-slot.v1.json";
const RENDEZVOUS_VERSION: u16 = 1;
const MAX_RENDEZVOUS_BYTES: usize = 1_024;
const NAME_ATTEMPTS: usize = 16;
const STAMP_DOMAIN: &[u8] = b"unity-asset:windows-pipe-rendezvous-stamp:v1\0";
const RENDEZVOUS_PUBLICATION: PublicationSlots = PublicationSlots::new(
    RENDEZVOUS_FILE,
    ".windows-pipe-slot-v1.staging",
    Some(".windows-pipe-slot-v1.quarantine"),
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PipeSlotId([u8; 16]);

impl PipeSlotId {
    pub(crate) fn generate() -> Result<Self, EndpointTransportError> {
        let mut random = rand::rngs::OsRng;
        for _ in 0..NAME_ATTEMPTS {
            let mut bytes = [0_u8; 16];
            random.try_fill_bytes(&mut bytes).map_err(|source| {
                EndpointTransportError::io(
                    "obtain entropy for Windows named-pipe slot",
                    io::Error::other(source),
                )
            })?;
            if bytes.iter().any(|byte| *byte != 0) {
                return Ok(Self(bytes));
            }
        }
        Err(EndpointTransportError::io(
            "obtain entropy for Windows named-pipe slot",
            io::Error::other("operating-system entropy repeatedly returned the zero slot ID"),
        ))
    }

    pub(crate) const fn as_bytes(self) -> [u8; 16] {
        self.0
    }

    fn is_zero(self) -> bool {
        self.0.iter().all(|byte| *byte == 0)
    }
}

impl Serialize for PipeSlotId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> Deserialize<'de> for PipeSlotId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = <&str>::deserialize(deserializer)?;
        if encoded.len() != 32 {
            return Err(serde::de::Error::custom(
                "Windows pipe slot ID must contain exactly 32 hexadecimal characters",
            ));
        }
        let mut bytes = [0_u8; 16];
        hex::decode_to_slice(encoded, &mut bytes).map_err(serde::de::Error::custom)?;
        Ok(Self(bytes))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PipeRendezvousV1 {
    rendezvous_version: u16,
    project_id: ProjectId,
    daemon_instance_id: DaemonInstanceId,
    security_context_id: SecurityContextIdV1,
    sequence: u64,
    slot_id: PipeSlotId,
}

impl PipeRendezvousV1 {
    pub(crate) fn initial(
        namespace: &EndpointNamespaceV1,
        daemon_instance_id: DaemonInstanceId,
        slot_id: PipeSlotId,
    ) -> Result<Self, EndpointTransportError> {
        let rendezvous = Self {
            rendezvous_version: RENDEZVOUS_VERSION,
            project_id: namespace.project_id(),
            daemon_instance_id,
            security_context_id: namespace.security_context_id(),
            sequence: 1,
            slot_id,
        };
        rendezvous.validate_namespace(namespace)?;
        Ok(rendezvous)
    }

    pub(crate) fn next(self, slot_id: PipeSlotId) -> Result<Self, EndpointTransportError> {
        let sequence = self.sequence.checked_add(1).ok_or_else(|| {
            EndpointTransportError::io(
                "advance Windows named-pipe rendezvous sequence",
                io::Error::other("Windows named-pipe rendezvous sequence exhausted"),
            )
        })?;
        Ok(Self {
            sequence,
            slot_id,
            ..self
        })
    }

    #[cfg(test)]
    pub(crate) const fn sequence(self) -> u64 {
        self.sequence
    }

    pub(crate) const fn slot_id(self) -> PipeSlotId {
        self.slot_id
    }

    fn validate_namespace(
        self,
        namespace: &EndpointNamespaceV1,
    ) -> Result<(), EndpointTransportError> {
        if self.rendezvous_version != RENDEZVOUS_VERSION {
            return Err(unsafe_rendezvous(
                "unsupported Windows pipe rendezvous version",
            ));
        }
        if self.project_id != namespace.project_id()
            || self.security_context_id != namespace.security_context_id()
        {
            return Err(unsafe_rendezvous(
                "Windows pipe rendezvous binding does not match its private namespace",
            ));
        }
        if self
            .daemon_instance_id
            .as_bytes()
            .iter()
            .all(|byte| *byte == 0)
            || self.sequence == 0
            || self.slot_id.is_zero()
        {
            return Err(unsafe_rendezvous(
                "Windows pipe rendezvous contains a reserved zero value",
            ));
        }
        Ok(())
    }

    fn validate_descriptor(
        self,
        namespace: &EndpointNamespaceV1,
        descriptor: EndpointDescriptorV1,
    ) -> Result<(), EndpointTransportError> {
        self.validate_namespace(namespace)?;
        if self.daemon_instance_id != descriptor.daemon_instance_id()
            || self.project_id != descriptor.project_id()
            || self.security_context_id != descriptor.security_context_id()
        {
            return Err(unsafe_rendezvous(
                "Windows pipe rendezvous does not match the endpoint descriptor",
            ));
        }
        Ok(())
    }
}

pub(crate) struct PublishedPipeRendezvousV1 {
    namespace: EndpointNamespaceV1,
    current: PipeRendezvousV1,
    stamp: RendezvousStamp,
    active: bool,
}

impl PublishedPipeRendezvousV1 {
    pub(crate) fn publish(
        namespace: &EndpointNamespaceV1,
        rendezvous: PipeRendezvousV1,
    ) -> Result<Self, EndpointTransportError> {
        rendezvous.validate_namespace(namespace)?;
        let encoded = encode(rendezvous)?;
        let stamp = RendezvousStamp::for_encoded(&encoded);
        let prepared = publication::prepare(namespace, RENDEZVOUS_PUBLICATION, &encoded).map_err(
            |source| {
                EndpointTransportError::io("create Windows pipe rendezvous staging file", source)
            },
        )?;
        let _ = prepared.commit(namespace).map_err(|source| {
            EndpointTransportError::io("atomically replace Windows pipe rendezvous", source)
        })?;
        let publication = Self {
            namespace: namespace.clone(),
            current: rendezvous,
            stamp,
            active: true,
        };
        if let Err(error) = publication.verify_current() {
            let _ = publication.remove_if_current();
            return Err(error);
        }
        Ok(publication)
    }

    pub(crate) fn rotate(&mut self, next: PipeRendezvousV1) -> Result<(), EndpointTransportError> {
        next.validate_namespace(&self.namespace)?;
        if next.project_id != self.current.project_id
            || next.daemon_instance_id != self.current.daemon_instance_id
            || next.security_context_id != self.current.security_context_id
            || next.sequence != self.current.sequence.checked_add(1).unwrap_or(0)
        {
            return Err(unsafe_rendezvous(
                "Windows pipe rendezvous rotation is not the next bound sequence",
            ));
        }
        let encoded = encode(next)?;
        let stamp = RendezvousStamp::for_encoded(&encoded);
        let prepared = publication::prepare(&self.namespace, RENDEZVOUS_PUBLICATION, &encoded)
            .map_err(|source| {
                EndpointTransportError::io("create Windows pipe rendezvous staging file", source)
            })?;
        let _ = prepared.commit(&self.namespace).map_err(|source| {
            EndpointTransportError::io("atomically replace Windows pipe rendezvous", source)
        })?;

        // The atomic replace is the commit point. Adopt ownership before readback so Drop can
        // conditionally remove the newly committed record if verification fails.
        self.current = next;
        self.stamp = stamp;
        self.verify_current()
    }

    pub(crate) const fn current(&self) -> PipeRendezvousV1 {
        self.current
    }

    pub(crate) fn remove(mut self) -> Result<EndpointCleanupV1, EndpointTransportError> {
        let result = self.remove_if_current()?;
        self.active = false;
        Ok(result)
    }

    fn verify_current(&self) -> Result<(), EndpointTransportError> {
        let (encoded, observed) = read(&self.namespace)?;
        if observed != self.current || RendezvousStamp::for_encoded(&encoded) != self.stamp {
            return Err(unsafe_rendezvous(
                "Windows pipe rendezvous changed during publication verification",
            ));
        }
        Ok(())
    }

    fn remove_if_current(&self) -> Result<EndpointCleanupV1, EndpointTransportError> {
        let Some(quarantine) = claim(&self.namespace)? else {
            return Ok(EndpointCleanupV1::AlreadyAbsent);
        };
        let observed = match read_named(&self.namespace, quarantine.name()) {
            Ok(observed) => observed,
            Err(error) => {
                restore(&self.namespace, quarantine)?;
                return Err(error);
            }
        };
        if observed.1 != self.current || RendezvousStamp::for_encoded(&observed.0) != self.stamp {
            restore(&self.namespace, quarantine)?;
            return Ok(EndpointCleanupV1::ReplacedPreserved);
        }
        remove_named(&self.namespace, quarantine)?;
        Ok(EndpointCleanupV1::Removed)
    }
}

impl Drop for PublishedPipeRendezvousV1 {
    fn drop(&mut self) {
        if self.active {
            let _ = self.remove_if_current();
        }
    }
}

pub(crate) fn discover(
    namespace: &EndpointNamespaceV1,
    descriptor: EndpointDescriptorV1,
) -> Result<PipeRendezvousV1, EndpointTransportError> {
    let (_, rendezvous) = read(namespace)?;
    rendezvous.validate_descriptor(namespace, descriptor)?;
    Ok(rendezvous)
}

pub(crate) fn retire_stale(namespace: &EndpointNamespaceV1) -> io::Result<EndpointCleanupV1> {
    let recovery = publication::recover_abandoned(namespace, RENDEZVOUS_PUBLICATION)?;
    let Some(quarantine) = publication::claim_current(namespace, RENDEZVOUS_PUBLICATION)? else {
        return Ok(if recovery.removed_any() {
            EndpointCleanupV1::Removed
        } else {
            EndpointCleanupV1::AlreadyAbsent
        });
    };
    quarantine.remove(namespace)?;
    Ok(EndpointCleanupV1::Removed)
}

fn encode(rendezvous: PipeRendezvousV1) -> Result<Vec<u8>, EndpointTransportError> {
    let encoded = serde_json::to_vec(&rendezvous).map_err(invalid_json)?;
    if encoded.len() > MAX_RENDEZVOUS_BYTES {
        return Err(unsafe_rendezvous(
            "Windows pipe rendezvous exceeds its encoded byte limit",
        ));
    }
    Ok(encoded)
}

fn read(
    namespace: &EndpointNamespaceV1,
) -> Result<(Vec<u8>, PipeRendezvousV1), EndpointTransportError> {
    read_named(namespace, OsStr::new(RENDEZVOUS_FILE))
}

fn read_named(
    namespace: &EndpointNamespaceV1,
    name: &OsStr,
) -> Result<(Vec<u8>, PipeRendezvousV1), EndpointTransportError> {
    let mut file = namespace.open_file(name, false).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            EndpointTransportError::EndpointUnavailable
        } else {
            EndpointTransportError::io("open Windows pipe rendezvous", source)
        }
    })?;
    let mut encoded = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(u64::try_from(MAX_RENDEZVOUS_BYTES).expect("rendezvous limit fits u64") + 1)
        .read_to_end(&mut encoded)
        .map_err(|source| EndpointTransportError::io("read Windows pipe rendezvous", source))?;
    if encoded.len() > MAX_RENDEZVOUS_BYTES {
        return Err(unsafe_rendezvous(
            "Windows pipe rendezvous exceeds its encoded byte limit",
        ));
    }
    let rendezvous: PipeRendezvousV1 = serde_json::from_slice(&encoded).map_err(invalid_json)?;
    if encode(rendezvous)?.as_slice() != encoded {
        return Err(unsafe_rendezvous(
            "Windows pipe rendezvous is not canonically encoded",
        ));
    }
    Ok((encoded, rendezvous))
}

fn claim(
    namespace: &EndpointNamespaceV1,
) -> Result<Option<QuarantinedPublication>, EndpointTransportError> {
    publication::claim_current(namespace, RENDEZVOUS_PUBLICATION)
        .map_err(|source| EndpointTransportError::io("claim Windows pipe rendezvous", source))
}

fn restore(
    namespace: &EndpointNamespaceV1,
    quarantine: QuarantinedPublication,
) -> Result<(), EndpointTransportError> {
    match quarantine.restore(namespace) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(EndpointTransportError::io(
            "restore non-owned Windows pipe rendezvous",
            source,
        )),
    }
}

fn remove_named(
    namespace: &EndpointNamespaceV1,
    quarantine: QuarantinedPublication,
) -> Result<(), EndpointTransportError> {
    match quarantine.remove(namespace) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(EndpointTransportError::io(
            "remove claimed Windows pipe rendezvous",
            source,
        )),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RendezvousStamp([u8; 32]);

impl RendezvousStamp {
    fn for_encoded(encoded: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(STAMP_DOMAIN);
        hasher.update(
            u64::try_from(encoded.len())
                .expect("bounded rendezvous length fits u64")
                .to_le_bytes(),
        );
        hasher.update(encoded);
        Self(hasher.finalize().into())
    }
}

fn invalid_json(source: serde_json::Error) -> EndpointTransportError {
    EndpointTransportError::io(
        "decode canonical Windows pipe rendezvous",
        io::Error::new(io::ErrorKind::InvalidData, source),
    )
}

fn unsafe_rendezvous(reason: &'static str) -> EndpointTransportError {
    EndpointTransportError::UnsafeEndpoint { reason }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;
    use crate::PrivateRootsV1;

    #[test]
    fn slot_id_is_fixed_length_canonical_hex() {
        let slot = PipeSlotId([0xab; 16]);
        let encoded = serde_json::to_string(&slot).unwrap();
        assert_eq!(encoded, format!("\"{}\"", "ab".repeat(16)));
        assert_eq!(serde_json::from_str::<PipeSlotId>(&encoded).unwrap(), slot);
        assert!(serde_json::from_str::<PipeSlotId>("\"ab\"").is_err());
        assert!(serde_json::from_str::<PipeSlotId>(&format!("\"{}\"", "AB".repeat(16))).is_ok());
    }

    #[test]
    fn stale_recovery_reclaims_rendezvous_staging_and_quarantine() {
        let roots = PrivateRootsV1::discover_for_current_context().unwrap();
        let mut project_bytes = rand::random::<[u8; 32]>();
        project_bytes[0] |= 1;
        let namespace = roots
            .runtime()
            .endpoint_namespace(ProjectId::from_bytes(project_bytes))
            .unwrap();
        let cleanup_path = namespace.path().to_path_buf();
        for name in [
            ".windows-pipe-slot-v1.staging",
            ".windows-pipe-slot-v1.quarantine",
        ] {
            let mut file = namespace.create_file(OsStr::new(name)).unwrap();
            file.write_all(b"abandoned rendezvous publication").unwrap();
            file.sync_all().unwrap();
        }

        assert_eq!(
            retire_stale(&namespace).unwrap(),
            EndpointCleanupV1::Removed
        );
        assert!(!cleanup_path.join(".windows-pipe-slot-v1.staging").exists());
        assert!(
            !cleanup_path
                .join(".windows-pipe-slot-v1.quarantine")
                .exists()
        );

        drop(namespace);
        drop(roots);
        for name in ["binding.v1", ".binding-v1.lock"] {
            std::fs::remove_file(cleanup_path.join(name)).unwrap();
        }
        std::fs::remove_dir(cleanup_path).unwrap();
    }
}
