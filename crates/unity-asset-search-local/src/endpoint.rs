use std::fmt;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::num::{NonZeroU16, NonZeroU32};
use std::str::FromStr;

use rand::TryRngCore as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use subtle::ConstantTimeEq as _;
use thiserror::Error;
use unity_asset_search_protocol::{
    BOOTSTRAP_VERSION, BUSINESS_PROTOCOL_REVISION, DaemonInstanceId, ProjectId, QueryPolicyId,
};

use crate::ids::{
    LocalIdentityParseError, deserialize_fixed_id, format_fixed_id, parse_fixed_id,
    serialize_fixed_id, validate_nonzero,
};
use crate::{ProcessIdentityError, ProcessIdentityV1, SecurityContextIdV1};

pub const ENDPOINT_DESCRIPTOR_VERSION: u16 = 1;
pub const MAX_ENDPOINT_DESCRIPTOR_BYTES: usize = 4 * 1024;
pub const LOOPBACK_ENDPOINT_DESCRIPTOR_VERSION: u16 = 2;
pub const HTTP_CAPABILITY_BYTES: usize = 32;
pub const HTTP_CAPABILITY_HEX_BYTES: usize = HTTP_CAPABILITY_BYTES * 2;
pub const MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES: usize = 512;
const PROCESS_START_PREFIX: &str = "process-start-v1:";
const LOCAL_IDENTITY_BYTES: usize = 32;
const HTTP_CAPABILITY_GENERATION_ATTEMPTS: usize = 16;

macro_rules! define_local_identity {
    ($name:ident, $prefix:expr) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; LOCAL_IDENTITY_BYTES]);

        impl $name {
            pub fn from_bytes(
                bytes: [u8; LOCAL_IDENTITY_BYTES],
            ) -> Result<Self, LocalIdentityParseError> {
                validate_nonzero(bytes).map(Self)
            }

            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; LOCAL_IDENTITY_BYTES] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                format_fixed_id(formatter, $prefix, &self.0)
            }
        }

        impl FromStr for $name {
            type Err = LocalIdentityParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_fixed_id(value, $prefix).map(Self)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serialize_fixed_id(serializer, $prefix, &self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                deserialize_fixed_id(deserializer, $prefix).map(Self)
            }
        }
    };
}

define_local_identity!(ProcessStartIdentityV1, PROCESS_START_PREFIX);

/// A process-instance bearer capability for the loopback HTTP boundary.
///
/// The type deliberately has no `Display` implementation, and its `Debug` representation never
/// reveals the credential. Equality and [`Self::matches`] use a fixed-size constant-time compare.
#[derive(Clone)]
pub struct HttpCapability([u8; HTTP_CAPABILITY_BYTES]);

impl HttpCapability {
    /// Generates a fresh capability from the operating system CSPRNG.
    pub fn generate() -> Result<Self, HttpCapabilityError> {
        let mut random = rand::rngs::OsRng;
        for _ in 0..HTTP_CAPABILITY_GENERATION_ATTEMPTS {
            let mut bytes = [0_u8; HTTP_CAPABILITY_BYTES];
            random.try_fill_bytes(&mut bytes).map_err(|source| {
                HttpCapabilityError::EntropyUnavailable {
                    message: source.to_string(),
                }
            })?;
            if let Ok(capability) = Self::from_bytes(bytes) {
                return Ok(capability);
            }
        }
        Err(HttpCapabilityError::EntropyReturnedOnlyZeroValues)
    }

    pub fn from_bytes(bytes: [u8; HTTP_CAPABILITY_BYTES]) -> Result<Self, HttpCapabilityError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(HttpCapabilityError::ZeroValue);
        }
        Ok(Self(bytes))
    }

    pub fn from_hex(encoded: &str) -> Result<Self, HttpCapabilityError> {
        if encoded.len() != HTTP_CAPABILITY_HEX_BYTES {
            return Err(HttpCapabilityError::InvalidLength {
                expected: HTTP_CAPABILITY_HEX_BYTES,
                actual: encoded.len(),
            });
        }
        if !encoded
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(HttpCapabilityError::InvalidEncoding);
        }
        let mut bytes = [0_u8; HTTP_CAPABILITY_BYTES];
        hex::decode_to_slice(encoded, &mut bytes)
            .map_err(|_| HttpCapabilityError::InvalidEncoding)?;
        Self::from_bytes(bytes)
    }

    #[must_use]
    pub fn encode_hex(&self) -> [u8; HTTP_CAPABILITY_HEX_BYTES] {
        let mut encoded = [0_u8; HTTP_CAPABILITY_HEX_BYTES];
        hex::encode_to_slice(self.0, &mut encoded)
            .expect("a fixed-size destination always fits a fixed-size capability");
        encoded
    }

    #[must_use]
    pub fn matches(&self, candidate: &Self) -> bool {
        bool::from(self.0.ct_eq(&candidate.0))
    }
}

impl fmt::Debug for HttpCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HttpCapability(<redacted>)")
    }
}

impl PartialEq for HttpCapability {
    fn eq(&self, other: &Self) -> bool {
        self.matches(other)
    }
}

impl Eq for HttpCapability {}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HttpCapabilityError {
    #[error("could not obtain operating-system entropy: {message}")]
    EntropyUnavailable { message: String },
    #[error("operating-system entropy repeatedly returned the reserved zero capability")]
    EntropyReturnedOnlyZeroValues,
    #[error("capability length is {actual}; expected {expected} lowercase hexadecimal bytes")]
    InvalidLength { expected: usize, actual: usize },
    #[error("capability must be lowercase hexadecimal")]
    InvalidEncoding,
    #[error("capability must not be all zeroes")]
    ZeroValue,
}

/// Secret discovery metadata for one loopback HTTP daemon process instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopbackEndpointDescriptor {
    project_id: ProjectId,
    daemon_instance_id: DaemonInstanceId,
    port: NonZeroU16,
    capability: HttpCapability,
    business_protocol_revision: u16,
    query_policy_id: QueryPolicyId,
    server_pid: NonZeroU32,
}

impl LoopbackEndpointDescriptor {
    pub fn for_current_process(
        project_id: ProjectId,
        daemon_instance_id: DaemonInstanceId,
        port: u16,
        capability: HttpCapability,
        query_policy_id: QueryPolicyId,
    ) -> Result<Self, LoopbackEndpointDescriptorError> {
        Self::new(
            project_id,
            daemon_instance_id,
            port,
            capability,
            query_policy_id,
            std::process::id(),
        )
    }

    pub fn new(
        project_id: ProjectId,
        daemon_instance_id: DaemonInstanceId,
        port: u16,
        capability: HttpCapability,
        query_policy_id: QueryPolicyId,
        server_pid: u32,
    ) -> Result<Self, LoopbackEndpointDescriptorError> {
        validate_loopback_protocol_id("project_id", project_id.as_bytes())?;
        validate_loopback_protocol_id("daemon_instance_id", daemon_instance_id.as_bytes())?;
        validate_loopback_protocol_id("query_policy_id", query_policy_id.as_bytes())?;
        let port = NonZeroU16::new(port)
            .ok_or(LoopbackEndpointDescriptorError::ZeroField { field: "port" })?;
        let server_pid =
            NonZeroU32::new(server_pid).ok_or(LoopbackEndpointDescriptorError::ZeroField {
                field: "server_pid",
            })?;
        Ok(Self {
            project_id,
            daemon_instance_id,
            port,
            capability,
            business_protocol_revision: BUSINESS_PROTOCOL_REVISION,
            query_policy_id,
            server_pid,
        })
    }

    #[must_use]
    pub const fn descriptor_version(&self) -> u16 {
        LOOPBACK_ENDPOINT_DESCRIPTOR_VERSION
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    pub const fn daemon_instance_id(&self) -> DaemonInstanceId {
        self.daemon_instance_id
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port.get()
    }

    #[must_use]
    pub const fn capability(&self) -> &HttpCapability {
        &self.capability
    }

    #[must_use]
    pub const fn business_protocol_revision(&self) -> u16 {
        self.business_protocol_revision
    }

    #[must_use]
    pub const fn query_policy_id(&self) -> QueryPolicyId {
        self.query_policy_id
    }

    #[must_use]
    pub const fn server_pid(&self) -> u32 {
        self.server_pid.get()
    }

    /// Returns the only address clients may derive from the descriptor.
    #[must_use]
    pub const fn socket_addr(&self) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, self.port.get())
    }

    pub fn validate_binding(
        &self,
        expected_project: ProjectId,
        expected_query_policy: QueryPolicyId,
    ) -> Result<(), LoopbackEndpointDescriptorError> {
        self.validate_project(expected_project)?;
        if self.query_policy_id != expected_query_policy {
            return Err(LoopbackEndpointDescriptorError::BindingMismatch {
                field: "query_policy_id",
            });
        }
        Ok(())
    }

    pub fn encode_json(&self) -> Result<Vec<u8>, LoopbackEndpointDescriptorError> {
        let encoded = serde_json::to_vec(&LoopbackEndpointDescriptorWire::from(self))?;
        if encoded.len() > MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES {
            return Err(LoopbackEndpointDescriptorError::EncodedSizeLimit {
                actual: encoded.len(),
                maximum: MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES,
            });
        }
        Ok(encoded)
    }

    pub fn decode_json(encoded: &[u8]) -> Result<Self, LoopbackEndpointDescriptorError> {
        if encoded.len() > MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES {
            return Err(LoopbackEndpointDescriptorError::EncodedSizeLimit {
                actual: encoded.len(),
                maximum: MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES,
            });
        }

        let mut version_deserializer = serde_json::Deserializer::from_slice(encoded);
        let version = EndpointDescriptorVersionProbe::deserialize(&mut version_deserializer)?;
        version_deserializer.end()?;
        if version.descriptor_version != LOOPBACK_ENDPOINT_DESCRIPTOR_VERSION {
            return Err(
                LoopbackEndpointDescriptorError::UnsupportedDescriptorVersion {
                    actual: version.descriptor_version,
                },
            );
        }

        let mut deserializer = serde_json::Deserializer::from_slice(encoded);
        let wire = LoopbackEndpointDescriptorWire::deserialize(&mut deserializer)?;
        deserializer.end()?;
        let descriptor = Self::try_from(wire)?;
        if descriptor.encode_json()?.as_slice() != encoded {
            return Err(LoopbackEndpointDescriptorError::NonCanonicalJson);
        }
        Ok(descriptor)
    }

    pub(crate) fn validate_project(
        &self,
        expected_project: ProjectId,
    ) -> Result<(), LoopbackEndpointDescriptorError> {
        if self.project_id != expected_project {
            return Err(LoopbackEndpointDescriptorError::BindingMismatch {
                field: "project_id",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum LoopbackEndpointDescriptorError {
    #[error("loopback endpoint descriptor is {actual} bytes; maximum is {maximum}")]
    EncodedSizeLimit { actual: usize, maximum: usize },
    #[error("unsupported loopback endpoint descriptor version {actual}")]
    UnsupportedDescriptorVersion { actual: u16 },
    #[error("unsupported business protocol revision {actual}; this build requires {expected}")]
    UnsupportedBusinessProtocolRevision { actual: u16, expected: u16 },
    #[error("loopback endpoint descriptor field {field} must not be zero")]
    ZeroField { field: &'static str },
    #[error("loopback endpoint descriptor does not match expected {field}")]
    BindingMismatch { field: &'static str },
    #[error("invalid loopback endpoint descriptor JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("loopback endpoint descriptor JSON is not in its canonical representation")]
    NonCanonicalJson,
}

#[derive(Deserialize)]
struct EndpointDescriptorVersionProbe {
    descriptor_version: u16,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoopbackEndpointDescriptorWire {
    descriptor_version: u16,
    project_id: ProjectId,
    daemon_instance_id: DaemonInstanceId,
    port: u16,
    #[serde(with = "http_capability_wire")]
    capability: HttpCapability,
    business_protocol_revision: u16,
    query_policy_id: QueryPolicyId,
    server_pid: u32,
}

mod http_capability_wire {
    use serde::{Deserialize as _, Deserializer, Serializer};

    use super::HttpCapability;

    pub(super) fn serialize<S>(
        capability: &HttpCapability,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = capability.encode_hex();
        let encoded = std::str::from_utf8(&encoded)
            .expect("hexadecimal capability encoding is always valid UTF-8");
        serializer.serialize_str(encoded)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<HttpCapability, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        HttpCapability::from_hex(&encoded).map_err(serde::de::Error::custom)
    }
}

impl From<&LoopbackEndpointDescriptor> for LoopbackEndpointDescriptorWire {
    fn from(descriptor: &LoopbackEndpointDescriptor) -> Self {
        Self {
            descriptor_version: LOOPBACK_ENDPOINT_DESCRIPTOR_VERSION,
            project_id: descriptor.project_id,
            daemon_instance_id: descriptor.daemon_instance_id,
            port: descriptor.port.get(),
            capability: descriptor.capability.clone(),
            business_protocol_revision: descriptor.business_protocol_revision,
            query_policy_id: descriptor.query_policy_id,
            server_pid: descriptor.server_pid.get(),
        }
    }
}

impl TryFrom<LoopbackEndpointDescriptorWire> for LoopbackEndpointDescriptor {
    type Error = LoopbackEndpointDescriptorError;

    fn try_from(wire: LoopbackEndpointDescriptorWire) -> Result<Self, Self::Error> {
        if wire.descriptor_version != LOOPBACK_ENDPOINT_DESCRIPTOR_VERSION {
            return Err(Self::Error::UnsupportedDescriptorVersion {
                actual: wire.descriptor_version,
            });
        }
        if wire.business_protocol_revision != BUSINESS_PROTOCOL_REVISION {
            return Err(Self::Error::UnsupportedBusinessProtocolRevision {
                actual: wire.business_protocol_revision,
                expected: BUSINESS_PROTOCOL_REVISION,
            });
        }
        Self::new(
            wire.project_id,
            wire.daemon_instance_id,
            wire.port,
            wire.capability,
            wire.query_policy_id,
            wire.server_pid,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointDescriptorV1 {
    project_id: ProjectId,
    daemon_instance_id: DaemonInstanceId,
    server_pid: NonZeroU32,
    process_start_identity: ProcessStartIdentityV1,
    security_context_id: SecurityContextIdV1,
}

impl EndpointDescriptorV1 {
    pub fn for_current_process(
        project_id: ProjectId,
        daemon_instance_id: DaemonInstanceId,
    ) -> Result<Self, EndpointDescriptorError> {
        let process = ProcessIdentityV1::current()?;
        Self::new(
            project_id,
            daemon_instance_id,
            process.process_id(),
            process.process_start_identity(),
            process.security_context_id(),
        )
    }

    pub fn new(
        project_id: ProjectId,
        daemon_instance_id: DaemonInstanceId,
        server_pid: u32,
        process_start_identity: ProcessStartIdentityV1,
        security_context_id: SecurityContextIdV1,
    ) -> Result<Self, EndpointDescriptorError> {
        validate_protocol_id("project_id", project_id.as_bytes())?;
        validate_protocol_id("daemon_instance_id", daemon_instance_id.as_bytes())?;
        let server_pid = NonZeroU32::new(server_pid).ok_or(EndpointDescriptorError::ZeroField {
            field: "server_pid",
        })?;
        Ok(Self {
            project_id,
            daemon_instance_id,
            server_pid,
            process_start_identity,
            security_context_id,
        })
    }

    #[must_use]
    pub const fn descriptor_version(&self) -> u16 {
        ENDPOINT_DESCRIPTOR_VERSION
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    pub const fn daemon_instance_id(&self) -> DaemonInstanceId {
        self.daemon_instance_id
    }

    #[must_use]
    pub const fn server_pid(&self) -> u32 {
        self.server_pid.get()
    }

    #[must_use]
    pub const fn process_start_identity(&self) -> ProcessStartIdentityV1 {
        self.process_start_identity
    }

    #[must_use]
    pub const fn security_context_id(&self) -> SecurityContextIdV1 {
        self.security_context_id
    }

    #[must_use]
    pub const fn bootstrap_version(&self) -> u16 {
        BOOTSTRAP_VERSION
    }

    pub fn validate_binding(
        &self,
        expected_project: ProjectId,
        expected_security_context: SecurityContextIdV1,
    ) -> Result<(), EndpointDescriptorError> {
        if self.project_id != expected_project {
            return Err(EndpointDescriptorError::BindingMismatch {
                field: "project_id",
            });
        }
        if self.security_context_id != expected_security_context {
            return Err(EndpointDescriptorError::BindingMismatch {
                field: "security_context_id",
            });
        }
        Ok(())
    }

    pub fn validate_server_process(
        &self,
        process: ProcessIdentityV1,
    ) -> Result<(), EndpointDescriptorError> {
        if self.server_pid() != process.process_id() {
            return Err(EndpointDescriptorError::BindingMismatch {
                field: "server_pid",
            });
        }
        for (matches, field) in [
            (
                process.process_start_identity() == self.process_start_identity,
                "process_start_identity",
            ),
            (
                process.security_context_id() == self.security_context_id,
                "security_context_id",
            ),
        ] {
            if !matches {
                return Err(EndpointDescriptorError::BindingMismatch { field });
            }
        }
        Ok(())
    }

    pub fn encode_json(&self) -> Result<Vec<u8>, EndpointDescriptorError> {
        let encoded = serde_json::to_vec(&EndpointDescriptorWire::from(self))?;
        if encoded.len() > MAX_ENDPOINT_DESCRIPTOR_BYTES {
            return Err(EndpointDescriptorError::EncodedSizeLimit {
                actual: encoded.len(),
                maximum: MAX_ENDPOINT_DESCRIPTOR_BYTES,
            });
        }
        Ok(encoded)
    }

    pub fn decode_json(encoded: &[u8]) -> Result<Self, EndpointDescriptorError> {
        if encoded.len() > MAX_ENDPOINT_DESCRIPTOR_BYTES {
            return Err(EndpointDescriptorError::EncodedSizeLimit {
                actual: encoded.len(),
                maximum: MAX_ENDPOINT_DESCRIPTOR_BYTES,
            });
        }
        let mut deserializer = serde_json::Deserializer::from_slice(encoded);
        let wire = EndpointDescriptorWire::deserialize(&mut deserializer)?;
        deserializer.end()?;
        let descriptor = Self::try_from(wire)?;
        if descriptor.encode_json()?.as_slice() != encoded {
            return Err(EndpointDescriptorError::NonCanonicalJson);
        }
        Ok(descriptor)
    }
}

#[derive(Debug, Error)]
pub enum EndpointDescriptorError {
    #[error("endpoint descriptor is {actual} bytes; maximum is {maximum}")]
    EncodedSizeLimit { actual: usize, maximum: usize },
    #[error("unsupported endpoint descriptor version {actual}")]
    UnsupportedDescriptorVersion { actual: u16 },
    #[error("unsupported endpoint bootstrap version {actual}")]
    UnsupportedBootstrapVersion { actual: u16 },
    #[error("endpoint descriptor field {field} must not be zero")]
    ZeroField { field: &'static str },
    #[error("endpoint descriptor does not match expected {field}")]
    BindingMismatch { field: &'static str },
    #[error("could not inspect the endpoint process: {0}")]
    ProcessIdentity(#[from] ProcessIdentityError),
    #[error("invalid endpoint descriptor JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("endpoint descriptor JSON is not in its canonical representation")]
    NonCanonicalJson,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointDescriptorWire {
    descriptor_version: u16,
    project_id: ProjectId,
    daemon_instance_id: DaemonInstanceId,
    server_pid: u32,
    process_start_identity: ProcessStartIdentityV1,
    security_context_id: SecurityContextIdV1,
    bootstrap_version: u16,
}

impl From<&EndpointDescriptorV1> for EndpointDescriptorWire {
    fn from(descriptor: &EndpointDescriptorV1) -> Self {
        Self {
            descriptor_version: ENDPOINT_DESCRIPTOR_VERSION,
            project_id: descriptor.project_id,
            daemon_instance_id: descriptor.daemon_instance_id,
            server_pid: descriptor.server_pid.get(),
            process_start_identity: descriptor.process_start_identity,
            security_context_id: descriptor.security_context_id,
            bootstrap_version: BOOTSTRAP_VERSION,
        }
    }
}

impl TryFrom<EndpointDescriptorWire> for EndpointDescriptorV1 {
    type Error = EndpointDescriptorError;

    fn try_from(wire: EndpointDescriptorWire) -> Result<Self, Self::Error> {
        if wire.descriptor_version != ENDPOINT_DESCRIPTOR_VERSION {
            return Err(EndpointDescriptorError::UnsupportedDescriptorVersion {
                actual: wire.descriptor_version,
            });
        }
        if wire.bootstrap_version != BOOTSTRAP_VERSION {
            return Err(EndpointDescriptorError::UnsupportedBootstrapVersion {
                actual: wire.bootstrap_version,
            });
        }
        Self::new(
            wire.project_id,
            wire.daemon_instance_id,
            wire.server_pid,
            wire.process_start_identity,
            wire.security_context_id,
        )
    }
}

fn validate_protocol_id(field: &'static str, bytes: &[u8]) -> Result<(), EndpointDescriptorError> {
    if bytes.iter().all(|byte| *byte == 0) {
        Err(EndpointDescriptorError::ZeroField { field })
    } else {
        Ok(())
    }
}

fn validate_loopback_protocol_id(
    field: &'static str,
    bytes: &[u8],
) -> Result<(), LoopbackEndpointDescriptorError> {
    if bytes.iter().all(|byte| *byte == 0) {
        Err(LoopbackEndpointDescriptorError::ZeroField { field })
    } else {
        Ok(())
    }
}
