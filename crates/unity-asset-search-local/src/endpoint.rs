use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use unity_asset_search_protocol::{BOOTSTRAP_VERSION, DaemonInstanceId, ProjectId};

use crate::ids::{
    LocalIdentityParseError, deserialize_fixed_id, format_fixed_id, parse_fixed_id,
    serialize_fixed_id, validate_nonzero,
};
use crate::{ProcessIdentityError, ProcessIdentityV1, SecurityContextIdV1};

pub const ENDPOINT_DESCRIPTOR_VERSION: u16 = 1;
pub const MAX_ENDPOINT_DESCRIPTOR_BYTES: usize = 4 * 1024;
const PROCESS_START_PREFIX: &str = "process-start-v1:";
const LOCAL_IDENTITY_BYTES: usize = 32;

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
