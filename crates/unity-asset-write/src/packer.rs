use std::fmt;
use std::str::FromStr;

use thiserror::Error;

/// Semantic packing policy for a prepared container artifact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PackingPolicy {
    /// Retain the source container's signature, flags, and compression policy.
    #[default]
    Preserve,
    /// Emit an uncompressed container while retaining all other wire semantics.
    Uncompressed,
    Lz4,
    Lzma,
}

impl FromStr for PackingPolicy {
    type Err = PackingPolicyParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "preserve" => Ok(Self::Preserve),
            "uncompressed" => Ok(Self::Uncompressed),
            "lz4" => Ok(Self::Lz4),
            "lzma" => Ok(Self::Lzma),
            _ => Err(PackingPolicyParseError {
                value: value.to_owned(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unsupported packing policy {value:?}; expected preserve, uncompressed, lz4, or lzma")]
pub struct PackingPolicyParseError {
    value: String,
}

impl fmt::Display for PackingPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackingPolicy::Preserve => write!(f, "preserve"),
            PackingPolicy::Uncompressed => write!(f, "uncompressed"),
            PackingPolicy::Lz4 => write!(f, "lz4"),
            PackingPolicy::Lzma => write!(f, "lzma"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_policy_names_round_trip() {
        for policy in [
            PackingPolicy::Preserve,
            PackingPolicy::Uncompressed,
            PackingPolicy::Lz4,
            PackingPolicy::Lzma,
        ] {
            assert_eq!(policy.to_string().parse::<PackingPolicy>(), Ok(policy));
        }
    }

    #[test]
    fn obsolete_unitypy_names_are_rejected() {
        for value in ["original", "none"] {
            assert!(value.parse::<PackingPolicy>().is_err());
        }
    }
}
