use std::io::{self, Read};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Finite limits applied before parsing, allocation, recursion, or decompression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetLoadLimits {
    pub max_entries: u64,
    pub max_bytes: u64,
    pub max_depth: u32,
    pub max_members: u64,
    pub max_compressed_bytes: u64,
    pub max_decompressed_bytes: u64,
    pub max_expansion_ratio: u32,
}

impl Default for AssetLoadLimits {
    fn default() -> Self {
        Self {
            max_entries: 4_000_000,
            max_bytes: 8 * 1024 * 1024 * 1024,
            max_depth: 512,
            max_members: 1_000_000,
            max_compressed_bytes: 8 * 1024 * 1024 * 1024,
            max_decompressed_bytes: 16 * 1024 * 1024 * 1024,
            max_expansion_ratio: 1_024,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssetLoadUsage {
    pub entries: u64,
    pub bytes: u64,
    pub max_observed_depth: u32,
    pub members: u64,
    pub compressed_bytes: u64,
    pub decompressed_bytes: u64,
}

/// Mutable ledger shared by all readers participating in one load operation.
#[derive(Debug, Default)]
pub struct AssetLoadBudget {
    limits: AssetLoadLimits,
    usage: AssetLoadUsage,
}

impl AssetLoadBudget {
    pub fn new(limits: AssetLoadLimits) -> Result<Self, BudgetError> {
        validate_limit("entries", u128::from(limits.max_entries))?;
        validate_limit("bytes", u128::from(limits.max_bytes))?;
        validate_limit("depth", u128::from(limits.max_depth))?;
        validate_limit("members", u128::from(limits.max_members))?;
        validate_limit("compressed_bytes", u128::from(limits.max_compressed_bytes))?;
        validate_limit(
            "decompressed_bytes",
            u128::from(limits.max_decompressed_bytes),
        )?;
        validate_limit("expansion_ratio", u128::from(limits.max_expansion_ratio))?;
        Ok(Self {
            limits,
            usage: AssetLoadUsage::default(),
        })
    }

    pub fn consume_entries(&mut self, amount: u64) -> Result<(), BudgetError> {
        charge(
            "entries",
            &mut self.usage.entries,
            amount,
            self.limits.max_entries,
        )
    }

    pub fn consume_bytes(&mut self, amount: u64) -> Result<(), BudgetError> {
        charge(
            "bytes",
            &mut self.usage.bytes,
            amount,
            self.limits.max_bytes,
        )
    }

    pub fn observe_depth(&mut self, depth: u32) -> Result<(), BudgetError> {
        if depth > self.limits.max_depth {
            return Err(BudgetError::Exceeded {
                resource: "depth",
                limit: u64::from(self.limits.max_depth),
                requested: u64::from(depth),
            });
        }
        self.usage.max_observed_depth = self.usage.max_observed_depth.max(depth);
        Ok(())
    }

    pub fn consume_members(&mut self, amount: u64) -> Result<(), BudgetError> {
        charge(
            "members",
            &mut self.usage.members,
            amount,
            self.limits.max_members,
        )
    }

    #[must_use]
    pub fn begin_decompression(&mut self) -> DecompressionBudget<'_> {
        DecompressionBudget {
            load: self,
            usage: DecompressionUsage::default(),
        }
    }

    /// Deserializes an untrusted JSON document after enforcing the remaining byte budget.
    ///
    /// Type-level Serde visitors can bound decoded values, but a streaming JSON parser may
    /// allocate scratch space before those visitors run. This entry point bounds the encoded
    /// document first and should be used for persisted automation contracts.
    pub fn deserialize_json<T: DeserializeOwned>(
        &mut self,
        mut reader: impl Read,
    ) -> Result<T, BudgetedJsonError> {
        let mut encoded = Vec::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let amount = u64::try_from(read)
                .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
            self.consume_bytes(amount)?;
            encoded
                .try_reserve(read)
                .map_err(|error| BudgetedJsonError::AllocationFailed {
                    requested: read,
                    message: error.to_string(),
                })?;
            encoded.extend_from_slice(&buffer[..read]);
        }
        Ok(serde_json::from_slice(&encoded)?)
    }

    #[must_use]
    pub const fn limits(&self) -> AssetLoadLimits {
        self.limits
    }

    #[must_use]
    pub const fn usage(&self) -> AssetLoadUsage {
        self.usage
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecompressionUsage {
    pub compressed_bytes: u64,
    pub decompressed_bytes: u64,
}

/// Per-stream ratio ledger that atomically charges the enclosing load budget.
#[derive(Debug)]
pub struct DecompressionBudget<'a> {
    load: &'a mut AssetLoadBudget,
    usage: DecompressionUsage,
}

impl DecompressionBudget<'_> {
    pub fn consume(
        &mut self,
        compressed_bytes: u64,
        decompressed_bytes: u64,
    ) -> Result<(), BudgetError> {
        let stream_compressed = self
            .usage
            .compressed_bytes
            .checked_add(compressed_bytes)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "compressed_bytes",
            })?;
        let stream_decompressed = self
            .usage
            .decompressed_bytes
            .checked_add(decompressed_bytes)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "decompressed_bytes",
            })?;
        let allowed =
            u128::from(stream_compressed) * u128::from(self.load.limits.max_expansion_ratio);
        if u128::from(stream_decompressed) > allowed {
            return Err(BudgetError::ExpansionRatioExceeded {
                compressed_bytes: stream_compressed,
                decompressed_bytes: stream_decompressed,
                max_ratio: self.load.limits.max_expansion_ratio,
            });
        }

        let load_compressed = self
            .load
            .usage
            .compressed_bytes
            .checked_add(compressed_bytes)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "compressed_bytes",
            })?;
        let load_decompressed = self
            .load
            .usage
            .decompressed_bytes
            .checked_add(decompressed_bytes)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "decompressed_bytes",
            })?;
        if load_compressed > self.load.limits.max_compressed_bytes {
            return Err(BudgetError::Exceeded {
                resource: "compressed_bytes",
                limit: self.load.limits.max_compressed_bytes,
                requested: load_compressed,
            });
        }
        if load_decompressed > self.load.limits.max_decompressed_bytes {
            return Err(BudgetError::Exceeded {
                resource: "decompressed_bytes",
                limit: self.load.limits.max_decompressed_bytes,
                requested: load_decompressed,
            });
        }

        self.usage.compressed_bytes = stream_compressed;
        self.usage.decompressed_bytes = stream_decompressed;
        self.load.usage.compressed_bytes = load_compressed;
        self.load.usage.decompressed_bytes = load_decompressed;
        Ok(())
    }

    #[must_use]
    pub const fn usage(&self) -> DecompressionUsage {
        self.usage
    }
}

fn validate_limit(resource: &'static str, value: u128) -> Result<(), BudgetError> {
    if value == 0 {
        return Err(BudgetError::InvalidLimit { resource });
    }
    Ok(())
}

fn charge(
    resource: &'static str,
    used: &mut u64,
    amount: u64,
    limit: u64,
) -> Result<(), BudgetError> {
    let requested = used
        .checked_add(amount)
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    if requested > limit {
        return Err(BudgetError::Exceeded {
            resource,
            limit,
            requested,
        });
    }
    *used = requested;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BudgetError {
    #[error("asset load limit for {resource} must be nonzero")]
    InvalidLimit { resource: &'static str },
    #[error("asset load budget arithmetic overflow for {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("asset load budget exceeded for {resource}: requested {requested}, limit {limit}")]
    Exceeded {
        resource: &'static str,
        limit: u64,
        requested: u64,
    },
    #[error(
        "decompression expansion ratio exceeded: {decompressed_bytes} from {compressed_bytes} bytes, maximum {max_ratio}:1"
    )]
    ExpansionRatioExceeded {
        compressed_bytes: u64,
        decompressed_bytes: u64,
        max_ratio: u32,
    },
}

#[derive(Debug, Error)]
pub enum BudgetedJsonError {
    #[error("failed to read JSON contract: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("failed to reserve {requested} bytes for JSON contract: {message}")]
    AllocationFailed { requested: usize, message: String },
    #[error("invalid JSON contract: {0}")]
    Json(#[from] serde_json::Error),
}
