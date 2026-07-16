use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::bounded::BoundedString;

const DOMAIN: &[u8] = b"unity-asset:digest:v1\0";
const PREFIX: &str = "blake3-v1:";
const WIRE_LENGTH: usize = PREFIX.len() + DigestV1::BYTE_LEN * 2;

/// Versioned byte identity used by persisted workspace contracts.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DigestV1([u8; 32]);

/// Exact-length streaming builder for the `DigestV1` byte domain.
pub struct DigestV1Builder {
    hasher: blake3::Hasher,
    declared_length: u64,
    consumed_length: u64,
}

impl DigestV1Builder {
    const FRAME_LENGTH_BYTES: u64 = 8;

    #[must_use]
    pub fn new(declared_length: u64) -> Self {
        Self {
            hasher: DigestV1::hasher(declared_length),
            declared_length,
            consumed_length: 0,
        }
    }

    /// Returns the encoded length of a byte slice framed by its little-endian `u64` length.
    pub fn framed_len(bytes: &[u8]) -> Result<u64, DigestBuildError> {
        let payload_length = Self::byte_len(bytes)?;
        Self::FRAME_LENGTH_BYTES
            .checked_add(payload_length)
            .ok_or(DigestBuildError::LengthOverflow)
    }

    pub fn update(&mut self, bytes: &[u8]) -> Result<(), DigestBuildError> {
        let requested = self.requested_length(Self::byte_len(bytes)?)?;
        self.hasher.update(bytes);
        self.consumed_length = requested;
        Ok(())
    }

    /// Adds a byte slice after its little-endian `u64` length prefix.
    ///
    /// The complete frame is checked before the builder state changes.
    pub fn update_framed(&mut self, bytes: &[u8]) -> Result<(), DigestBuildError> {
        let payload_length = Self::byte_len(bytes)?;
        let requested = self.requested_length(
            Self::FRAME_LENGTH_BYTES
                .checked_add(payload_length)
                .ok_or(DigestBuildError::LengthOverflow)?,
        )?;
        self.hasher.update(&payload_length.to_le_bytes());
        self.hasher.update(bytes);
        self.consumed_length = requested;
        Ok(())
    }

    pub fn finalize(self) -> Result<DigestV1, DigestBuildError> {
        if self.consumed_length != self.declared_length {
            return Err(DigestBuildError::DeclaredLengthMismatch {
                declared: self.declared_length,
                consumed: self.consumed_length,
            });
        }
        Ok(DigestV1(*self.hasher.finalize().as_bytes()))
    }

    #[must_use]
    pub const fn consumed_bytes(&self) -> u64 {
        self.consumed_length
    }

    fn byte_len(bytes: &[u8]) -> Result<u64, DigestBuildError> {
        u64::try_from(bytes.len()).map_err(|_| DigestBuildError::LengthOverflow)
    }

    fn requested_length(&self, amount: u64) -> Result<u64, DigestBuildError> {
        let requested = self
            .consumed_length
            .checked_add(amount)
            .ok_or(DigestBuildError::LengthOverflow)?;
        if requested > self.declared_length {
            return Err(DigestBuildError::DeclaredLengthExceeded {
                declared: self.declared_length,
                requested,
            });
        }
        Ok(requested)
    }
}

impl DigestV1 {
    pub const BYTE_LEN: usize = 32;

    #[must_use]
    pub fn hash_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Self::hasher(bytes.len() as u64);
        hasher.update(bytes);
        Self(*hasher.finalize().as_bytes())
    }

    pub fn hash_reader(mut reader: impl Read, length: u64) -> io::Result<Self> {
        let mut hasher = Self::hasher(length);
        let mut remaining = length;
        let mut buffer = [0_u8; 64 * 1024];

        while remaining != 0 {
            let wanted = if remaining > buffer.len() as u64 {
                buffer.len()
            } else {
                remaining as usize
            };
            let read = reader.read(&mut buffer[..wanted])?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("digest input ended with {remaining} bytes remaining"),
                ));
            }
            hasher.update(&buffer[..read]);
            remaining -= read as u64;
        }

        let mut trailing = [0_u8; 1];
        if reader.read(&mut trailing)? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "digest input contains bytes after its declared logical length",
            ));
        }

        Ok(Self(*hasher.finalize().as_bytes()))
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; Self::BYTE_LEN]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; Self::BYTE_LEN] {
        &self.0
    }

    fn hasher(length: u64) -> blake3::Hasher {
        let mut hasher = blake3::Hasher::new();
        hasher.update(DOMAIN);
        hasher.update(&length.to_le_bytes());
        hasher
    }
}

impl fmt::Debug for DigestV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for DigestV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{PREFIX}{}", hex::encode(self.0))
    }
}

impl FromStr for DigestV1 {
    type Err = DigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let encoded = value
            .strip_prefix(PREFIX)
            .ok_or(DigestParseError::UnsupportedVersion)?;
        let expected = Self::BYTE_LEN * 2;
        if encoded.len() != expected {
            return Err(DigestParseError::InvalidEncodedLength {
                actual: encoded.len(),
                expected,
            });
        }
        let mut bytes = [0_u8; Self::BYTE_LEN];
        hex::decode_to_slice(encoded, &mut bytes).map_err(DigestParseError::InvalidHex)?;
        Ok(Self(bytes))
    }
}

impl Serialize for DigestV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for DigestV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = BoundedString::<WIRE_LENGTH>::deserialize(deserializer)?.into_string();
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error)]
pub enum DigestParseError {
    #[error("unsupported digest version")]
    UnsupportedVersion,
    #[error("digest has {actual} encoded characters; expected {expected}")]
    InvalidEncodedLength { actual: usize, expected: usize },
    #[error("digest is not valid hexadecimal: {0}")]
    InvalidHex(hex::FromHexError),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DigestBuildError {
    #[error("digest input length overflow")]
    LengthOverflow,
    #[error("digest input requested {requested} bytes; declared length is {declared}")]
    DeclaredLengthExceeded { declared: u64, requested: u64 },
    #[error("digest input consumed {consumed} bytes; declared length is {declared}")]
    DeclaredLengthMismatch { declared: u64, consumed: u64 },
}
