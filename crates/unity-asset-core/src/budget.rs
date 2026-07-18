use std::io::{self, Read};
use std::ops::{Deref, DerefMut};

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
    depth_base: u32,
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
            depth_base: 0,
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

    /// Checks whether entries can be visited without charging usage.
    pub fn check_entries(&self, amount: u64) -> Result<(), BudgetError> {
        check_charge(
            "entries",
            self.usage.entries,
            amount,
            self.limits.max_entries,
        )
        .map(|_| ())
    }

    pub fn consume_bytes(&mut self, amount: u64) -> Result<(), BudgetError> {
        charge(
            "bytes",
            &mut self.usage.bytes,
            amount,
            self.limits.max_bytes,
        )
    }

    /// Checks whether parser or codec scratch memory can be reserved without charging usage.
    ///
    /// Callers that proceed with the allocation must still charge it through
    /// [`Self::consume_bytes`]. The byte ledger is monotonic, so temporary allocations remain
    /// accounted for after they are released.
    pub fn check_bytes(&self, amount: u64) -> Result<(), BudgetError> {
        check_charge("bytes", self.usage.bytes, amount, self.limits.max_bytes).map(|_| ())
    }

    pub fn observe_depth(&mut self, depth: u32) -> Result<(), BudgetError> {
        let absolute = self.absolute_depth(depth)?;
        self.check_absolute_depth(absolute)?;
        self.usage.max_observed_depth = self.usage.max_observed_depth.max(absolute);
        Ok(())
    }

    /// Checks a recursion depth without changing the maximum observed depth.
    pub fn check_depth(&self, depth: u32) -> Result<(), BudgetError> {
        self.check_absolute_depth(self.absolute_depth(depth)?)
    }

    /// Enters a parser whose local depth zero starts at `base` below the current parser.
    ///
    /// Parsers continue to report local depths through [`Self::observe_depth`]. The scope adds the
    /// complete outer-container depth and restores the previous base when dropped.
    pub fn enter_depth(&mut self, base: u32) -> Result<AssetLoadDepthScope<'_>, BudgetError> {
        let previous = self.depth_base;
        let combined = previous
            .checked_add(base)
            .ok_or(BudgetError::ArithmeticOverflow { resource: "depth" })?;
        self.check_absolute_depth(combined)?;
        self.depth_base = combined;
        Ok(AssetLoadDepthScope {
            budget: self,
            previous,
        })
    }

    fn absolute_depth(&self, depth: u32) -> Result<u32, BudgetError> {
        self.depth_base
            .checked_add(depth)
            .ok_or(BudgetError::ArithmeticOverflow { resource: "depth" })
    }

    fn check_absolute_depth(&self, depth: u32) -> Result<(), BudgetError> {
        if depth > self.limits.max_depth {
            return Err(BudgetError::Exceeded {
                resource: "depth",
                limit: u64::from(self.limits.max_depth),
                requested: u64::from(depth),
            });
        }
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

    /// Checks whether collection members can be traversed without charging usage.
    pub fn check_members(&self, amount: u64) -> Result<(), BudgetError> {
        check_charge(
            "members",
            self.usage.members,
            amount,
            self.limits.max_members,
        )
        .map(|_| ())
    }

    /// Checks whether encoded input can be charged before it is copied or decoded.
    ///
    /// A successful check does not mutate usage. The decoder must still charge the bytes through
    /// [`Self::begin_decompression`] when it starts processing the stream.
    pub fn check_compressed_bytes(&self, amount: u64) -> Result<(), BudgetError> {
        check_charge(
            "compressed_bytes",
            self.usage.compressed_bytes,
            amount,
            self.limits.max_compressed_bytes,
        )
        .map(|_| ())
    }

    /// Checks a planned decompression against the remaining load limits without charging usage.
    ///
    /// The compressed and decompressed values describe one decoder stream. Its expansion ratio is
    /// checked independently so one stream cannot borrow allowance from earlier streams. A decoder
    /// must still charge its actual work through [`Self::begin_decompression`].
    pub fn check_decompression(
        &self,
        compressed_bytes: u64,
        decompressed_bytes: u64,
    ) -> Result<(), BudgetError> {
        checked_decompression_usage(
            self.usage,
            DecompressionUsage::default(),
            self.limits,
            compressed_bytes,
            decompressed_bytes,
        )
        .map(|_| ())
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

    /// Returns the byte allowance that has not yet been consumed by this load.
    #[must_use]
    pub const fn remaining_bytes(&self) -> u64 {
        self.limits.max_bytes - self.usage.bytes
    }
}

/// RAII scope that composes a parser's local depth with its owning container depth.
#[derive(Debug)]
#[must_use = "keep the scope alive while the nested parser uses the budget"]
pub struct AssetLoadDepthScope<'budget> {
    budget: &'budget mut AssetLoadBudget,
    previous: u32,
}

impl Deref for AssetLoadDepthScope<'_> {
    type Target = AssetLoadBudget;

    fn deref(&self) -> &Self::Target {
        self.budget
    }
}

impl DerefMut for AssetLoadDepthScope<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.budget
    }
}

impl Drop for AssetLoadDepthScope<'_> {
    fn drop(&mut self) {
        self.budget.depth_base = self.previous;
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
        let (load_usage, stream_usage) = checked_decompression_usage(
            self.load.usage,
            self.usage,
            self.load.limits,
            compressed_bytes,
            decompressed_bytes,
        )?;
        self.usage = stream_usage;
        self.load.usage = load_usage;
        Ok(())
    }

    #[must_use]
    pub const fn usage(&self) -> DecompressionUsage {
        self.usage
    }
}

fn checked_decompression_usage(
    load_usage: AssetLoadUsage,
    stream_usage: DecompressionUsage,
    limits: AssetLoadLimits,
    compressed_bytes: u64,
    decompressed_bytes: u64,
) -> Result<(AssetLoadUsage, DecompressionUsage), BudgetError> {
    let stream_compressed = stream_usage
        .compressed_bytes
        .checked_add(compressed_bytes)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "compressed_bytes",
        })?;
    let stream_decompressed = stream_usage
        .decompressed_bytes
        .checked_add(decompressed_bytes)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "decompressed_bytes",
        })?;
    let allowed = u128::from(stream_compressed) * u128::from(limits.max_expansion_ratio);
    if u128::from(stream_decompressed) > allowed {
        return Err(BudgetError::ExpansionRatioExceeded {
            compressed_bytes: stream_compressed,
            decompressed_bytes: stream_decompressed,
            max_ratio: limits.max_expansion_ratio,
        });
    }

    let next_load_compressed = load_usage
        .compressed_bytes
        .checked_add(compressed_bytes)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "compressed_bytes",
        })?;
    let next_load_decompressed = load_usage
        .decompressed_bytes
        .checked_add(decompressed_bytes)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "decompressed_bytes",
        })?;
    if next_load_compressed > limits.max_compressed_bytes {
        return Err(BudgetError::Exceeded {
            resource: "compressed_bytes",
            limit: limits.max_compressed_bytes,
            requested: next_load_compressed,
        });
    }
    if next_load_decompressed > limits.max_decompressed_bytes {
        return Err(BudgetError::Exceeded {
            resource: "decompressed_bytes",
            limit: limits.max_decompressed_bytes,
            requested: next_load_decompressed,
        });
    }

    Ok((
        AssetLoadUsage {
            compressed_bytes: next_load_compressed,
            decompressed_bytes: next_load_decompressed,
            ..load_usage
        },
        DecompressionUsage {
            compressed_bytes: stream_compressed,
            decompressed_bytes: stream_decompressed,
        },
    ))
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
    let requested = check_charge(resource, *used, amount, limit)?;
    *used = requested;
    Ok(())
}

fn check_charge(
    resource: &'static str,
    used: u64,
    amount: u64,
    limit: u64,
) -> Result<u64, BudgetError> {
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
    Ok(requested)
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
