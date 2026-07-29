use std::fmt;

use serde::{Deserialize, Deserializer, Serializer};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LocalIdentityParseError {
    #[error("invalid identity prefix; expected {expected}")]
    InvalidPrefix { expected: &'static str },
    #[error("invalid identity length {actual}; expected {expected}")]
    InvalidLength { expected: usize, actual: usize },
    #[error("identity payload must be lowercase hexadecimal")]
    InvalidEncoding,
    #[error("identity payload must not be all zeroes")]
    ZeroValue,
}

pub(crate) fn parse_fixed_id<const N: usize>(
    value: &str,
    prefix: &'static str,
) -> Result<[u8; N], LocalIdentityParseError> {
    let encoded = value
        .strip_prefix(prefix)
        .ok_or(LocalIdentityParseError::InvalidPrefix { expected: prefix })?;
    let expected_payload = N * 2;
    if encoded.len() != expected_payload {
        return Err(LocalIdentityParseError::InvalidLength {
            expected: prefix.len() + expected_payload,
            actual: value.len(),
        });
    }
    if !encoded
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(LocalIdentityParseError::InvalidEncoding);
    }
    let mut bytes = [0_u8; N];
    hex::decode_to_slice(encoded, &mut bytes)
        .map_err(|_| LocalIdentityParseError::InvalidEncoding)?;
    validate_nonzero(bytes)
}

pub(crate) fn validate_nonzero<const N: usize>(
    bytes: [u8; N],
) -> Result<[u8; N], LocalIdentityParseError> {
    if bytes.iter().all(|byte| *byte == 0) {
        Err(LocalIdentityParseError::ZeroValue)
    } else {
        Ok(bytes)
    }
}

pub(crate) fn format_fixed_id(
    formatter: &mut fmt::Formatter<'_>,
    prefix: &'static str,
    bytes: &[u8],
) -> fmt::Result {
    formatter.write_str(prefix)?;
    for byte in bytes {
        write!(formatter, "{byte:02x}")?;
    }
    Ok(())
}

pub(crate) fn serialize_fixed_id<S>(
    serializer: S,
    prefix: &'static str,
    bytes: &[u8],
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&format!("{prefix}{}", hex::encode(bytes)))
}

pub(crate) fn deserialize_fixed_id<'de, D, const N: usize>(
    deserializer: D,
    prefix: &'static str,
) -> Result<[u8; N], D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    parse_fixed_id(&value, prefix).map_err(serde::de::Error::custom)
}
