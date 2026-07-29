use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

macro_rules! define_fixed_id {
    ($name:ident, $prefix:literal, $bytes:literal) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; $bytes]);

        impl $name {
            #[must_use]
            pub const fn from_bytes(bytes: [u8; $bytes]) -> Self {
                Self(bytes)
            }

            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; $bytes] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str($prefix)?;
                formatter.write_str(":")?;
                formatter.write_str(&hex::encode(self.0))
            }
        }

        impl FromStr for $name {
            type Err = FixedIdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                const PREFIX: &str = concat!($prefix, ":");
                let encoded = value
                    .strip_prefix(PREFIX)
                    .ok_or(FixedIdParseError::InvalidPrefix { expected: PREFIX })?;
                let expected = $bytes * 2;
                if encoded.len() != expected {
                    return Err(FixedIdParseError::InvalidLength {
                        expected: PREFIX.len() + expected,
                        actual: value.len(),
                    });
                }
                if !encoded
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                {
                    return Err(FixedIdParseError::InvalidEncoding);
                }
                let mut bytes = [0_u8; $bytes];
                hex::decode_to_slice(encoded, &mut bytes)
                    .map_err(|_| FixedIdParseError::InvalidEncoding)?;
                Ok(Self(bytes))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                String::deserialize(deserializer)?
                    .parse()
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

define_fixed_id!(ProjectId, "project-v1", 32);
define_fixed_id!(DaemonInstanceId, "daemon-v1", 16);
define_fixed_id!(RequestId, "request-v1", 16);
define_fixed_id!(OperationId, "operation-v1", 16);
define_fixed_id!(QueryPolicyId, "query-policy-v1", 32);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FixedIdParseError {
    #[error("invalid identifier prefix; expected {expected}")]
    InvalidPrefix { expected: &'static str },
    #[error("invalid identifier length {actual}; expected {expected}")]
    InvalidLength { expected: usize, actual: usize },
    #[error("identifier payload must be lowercase hexadecimal")]
    InvalidEncoding,
}
