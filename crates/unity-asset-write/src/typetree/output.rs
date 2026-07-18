//! Budgeted append-only output for canonical TypeTree traversal.

use unity_asset_binary::typetree::TypeTreeTraversalStats;
use unity_asset_core::{AssetLoadBudget, BudgetError, Result, UnityAssetError};

use crate::binary_writer::Endian;

#[derive(Debug, Clone, Copy)]
struct PreparedAppend {
    new_len: usize,
    accounted_capacity: usize,
    combined_bytes: u64,
    stats: TypeTreeTraversalStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppendKind {
    Plain,
    Scalar,
    Bulk,
}

/// An append-only TypeTree output buffer with caller-owned resource accounting.
///
/// `Vec` may reserve more storage than requested. Only explicitly charged logical capacity may be
/// used by later writes, so allocator over-allocation never becomes free output capacity.
pub(crate) struct TypeTreeOutput<'budget> {
    bytes: Vec<u8>,
    endian: Endian,
    budget: &'budget mut AssetLoadBudget,
    stats: TypeTreeTraversalStats,
    accounted_capacity: usize,
}

impl<'budget> TypeTreeOutput<'budget> {
    pub(crate) fn new(endian: Endian, budget: &'budget mut AssetLoadBudget) -> Self {
        Self {
            bytes: Vec::new(),
            endian,
            budget,
            stats: TypeTreeTraversalStats::default(),
            accounted_capacity: 0,
        }
    }

    #[must_use]
    pub(crate) fn position(&self) -> u64 {
        self.stats.wire_bytes
    }

    #[must_use]
    pub(crate) fn finish(self) -> (Vec<u8>, TypeTreeTraversalStats) {
        (self.bytes, self.stats)
    }

    pub(crate) fn write_i32(&mut self, value: i32) -> Result<()> {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.write_bytes(&bytes)
    }

    pub(crate) fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.append(bytes, AppendKind::Plain)
    }

    /// Appends one scalar primitive and records the codec operation atomically.
    pub(crate) fn write_scalar_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.append(bytes, AppendKind::Scalar)
    }

    /// Appends one contiguous numeric payload and records it as a bulk traversal run.
    pub(crate) fn write_bulk_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.append(bytes, AppendKind::Bulk)
    }

    /// Pads the output to the next absolute alignment boundary.
    pub(crate) fn align_to(&mut self, alignment: usize) -> Result<()> {
        if alignment == 0 {
            return Err(UnityAssetError::format(
                "TypeTree output alignment must be nonzero",
            ));
        }
        let remainder = self.bytes.len() % alignment;
        let padding = if remainder == 0 {
            0
        } else {
            alignment - remainder
        };
        self.append_zeros(padding)
    }

    /// Accounts for entering one canonical schema node before recursive traversal.
    pub(crate) fn enter_node(&mut self, depth: u32) -> Result<()> {
        self.enter_nodes(depth, 1)
    }

    /// Accounts for repeated canonical nodes without requiring a scalar write loop.
    pub(crate) fn enter_nodes(&mut self, depth: u32, amount: u64) -> Result<()> {
        let node_visits = checked_add_metric(self.stats.node_visits, amount, "node visits")?;
        self.budget
            .check_depth(depth)
            .map_err(|error| budget_error("check TypeTree output depth", error))?;
        self.budget
            .check_entries(amount)
            .map_err(|error| budget_error("check TypeTree output node visits", error))?;
        self.budget
            .consume_entries(amount)
            .map_err(|error| budget_error("charge TypeTree output node visits", error))?;
        self.budget
            .observe_depth(depth)
            .map_err(|error| budget_error("record TypeTree output depth", error))?;
        self.stats.node_visits = node_visits;
        Ok(())
    }

    /// Accounts for members in a dynamic collection before writing their values.
    pub(crate) fn consume_members(&mut self, amount: u64) -> Result<()> {
        let members = checked_add_metric(self.stats.members, amount, "members")?;
        self.budget
            .check_members(amount)
            .map_err(|error| budget_error("check TypeTree output members", error))?;
        self.budget
            .consume_members(amount)
            .map_err(|error| budget_error("charge TypeTree output members", error))?;
        self.stats.members = members;
        Ok(())
    }

    fn append(&mut self, bytes: &[u8], kind: AppendKind) -> Result<()> {
        let prepared = self.prepare_append(bytes.len(), kind)?;
        self.preflight_and_reserve(prepared.accounted_capacity, prepared.combined_bytes)?;

        // The preflight makes this charge infallible while this output holds the only mutable
        // borrow of the budget. Keeping it after allocation preserves failure atomicity.
        self.budget
            .consume_bytes(prepared.combined_bytes)
            .map_err(|error| budget_error("charge TypeTree output bytes", error))?;
        self.bytes.extend_from_slice(bytes);
        debug_assert_eq!(self.bytes.len(), prepared.new_len);
        self.accounted_capacity = prepared.accounted_capacity;
        self.stats = prepared.stats;
        Ok(())
    }

    fn append_zeros(&mut self, count: usize) -> Result<()> {
        let prepared = self.prepare_append(count, AppendKind::Plain)?;
        self.preflight_and_reserve(prepared.accounted_capacity, prepared.combined_bytes)?;

        self.budget
            .consume_bytes(prepared.combined_bytes)
            .map_err(|error| budget_error("charge TypeTree output padding", error))?;
        self.bytes.resize(prepared.new_len, 0);
        self.accounted_capacity = prepared.accounted_capacity;
        self.stats = prepared.stats;
        Ok(())
    }

    fn prepare_append(&self, count: usize, kind: AppendKind) -> Result<PreparedAppend> {
        let new_len = self
            .bytes
            .len()
            .checked_add(count)
            .ok_or_else(|| UnityAssetError::format("TypeTree output length overflow"))?;
        let accounted_capacity = if new_len <= self.accounted_capacity {
            self.accounted_capacity
        } else if self.accounted_capacity == 0 {
            new_len
        } else {
            self.accounted_capacity
                .checked_mul(2)
                .ok_or_else(|| UnityAssetError::format("TypeTree output capacity overflow"))?
                .max(new_len)
        };
        let added_capacity = accounted_capacity
            .checked_sub(self.accounted_capacity)
            .ok_or_else(|| UnityAssetError::format("TypeTree output capacity ledger is invalid"))?;
        let wire = u64::try_from(count)
            .map_err(|_| UnityAssetError::format("TypeTree output extent does not fit in u64"))?;
        let owned = u64::try_from(added_capacity)
            .map_err(|_| UnityAssetError::format("TypeTree output capacity does not fit in u64"))?;
        let combined_bytes = wire.checked_add(owned).ok_or_else(|| {
            UnityAssetError::format("TypeTree output wire and owned byte total overflow")
        })?;

        let mut stats = self.stats;
        stats.wire_bytes = checked_add_metric(stats.wire_bytes, wire, "wire bytes")?;
        stats.owned_bytes = checked_add_metric(stats.owned_bytes, owned, "owned bytes")?;
        match kind {
            AppendKind::Plain => {}
            AppendKind::Scalar => {
                stats.scalar_element_ops =
                    checked_add_metric(stats.scalar_element_ops, 1, "scalar element operations")?;
            }
            AppendKind::Bulk if count != 0 => {
                stats.bulk_runs = checked_add_metric(stats.bulk_runs, 1, "bulk runs")?;
                stats.bulk_bytes = checked_add_metric(stats.bulk_bytes, wire, "bulk bytes")?;
            }
            AppendKind::Bulk => {}
        }

        Ok(PreparedAppend {
            new_len,
            accounted_capacity,
            combined_bytes,
            stats,
        })
    }

    fn preflight_and_reserve(&mut self, target_capacity: usize, combined_bytes: u64) -> Result<()> {
        self.budget
            .check_bytes(combined_bytes)
            .map_err(|error| budget_error("check TypeTree output bytes", error))?;
        let reserve = target_capacity
            .checked_sub(self.bytes.len())
            .ok_or_else(|| UnityAssetError::format("TypeTree output capacity ledger is invalid"))?;
        self.bytes.try_reserve_exact(reserve).map_err(|error| {
            UnityAssetError::with_source(
                format!("failed to reserve {reserve} bytes for TypeTree output"),
                error,
            )
        })?;
        Ok(())
    }
}

fn checked_add_metric(current: u64, amount: u64, label: &str) -> Result<u64> {
    current
        .checked_add(amount)
        .ok_or_else(|| UnityAssetError::format(format!("TypeTree output {label} metric overflow")))
}

fn budget_error(operation: &'static str, error: BudgetError) -> UnityAssetError {
    UnityAssetError::with_source(operation, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::{AssetLoadLimits, AssetLoadUsage};

    fn budget_with_limits(bytes: u64, entries: u64, members: u64, depth: u32) -> AssetLoadBudget {
        AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: bytes,
            max_entries: entries,
            max_members: members,
            max_depth: depth,
            ..AssetLoadLimits::default()
        })
        .unwrap()
    }

    fn budget_with_bytes(bytes: u64) -> AssetLoadBudget {
        budget_with_limits(bytes, 64, 64, 16)
    }

    #[test]
    fn alignment_appends_budgeted_zero_padding() {
        let mut budget = budget_with_bytes(16);
        let mut output = TypeTreeOutput::new(Endian::Little, &mut budget);
        output.write_bytes(&[0xaa]).unwrap();
        output.align_to(4).unwrap();
        output.align_to(4).unwrap();

        assert_eq!(output.position(), 4);
        assert_eq!(output.bytes, [0xaa, 0, 0, 0]);
        assert_eq!(output.stats.wire_bytes, 4);
        assert_eq!(output.stats.owned_bytes, 4);
        let _ = output.finish();
        assert_eq!(budget.usage().bytes, 8);
    }

    #[test]
    fn budget_failure_does_not_change_output_stats_or_usage() {
        let mut budget = budget_with_bytes(4);
        let mut output = TypeTreeOutput::new(Endian::Little, &mut budget);
        output.write_bytes(&[7]).unwrap();

        let before_bytes = output.bytes.clone();
        let before_stats = output.stats;
        assert!(output.write_bytes(&[8, 9]).is_err());
        assert_eq!(output.bytes, before_bytes);
        assert_eq!(output.stats, before_stats);
        assert_eq!(output.position(), 1);
        let _ = output.finish();
        assert_eq!(budget.usage().bytes, 2);
    }

    #[test]
    fn allocator_spare_capacity_is_never_free_logical_capacity() {
        let mut budget = budget_with_bytes(32);
        let mut output = TypeTreeOutput::new(Endian::Little, &mut budget);
        output.bytes.reserve_exact(64);
        assert!(output.bytes.capacity() >= 64);
        assert_eq!(output.accounted_capacity, 0);

        output.write_bytes(&[1]).unwrap();
        output.write_bytes(&[2, 3, 4]).unwrap();

        assert_eq!(output.accounted_capacity, 4);
        assert_eq!(output.stats.owned_bytes, 4);
        let _ = output.finish();
        assert_eq!(budget.usage().bytes, 8);
    }

    #[test]
    fn repeated_tiny_writes_grow_logical_capacity_geometrically() {
        let mut budget = budget_with_bytes(13);
        let mut output = TypeTreeOutput::new(Endian::Little, &mut budget);

        for (value, expected_capacity) in [1_u8, 2, 3, 4, 5].into_iter().zip([1_usize, 2, 4, 4, 8])
        {
            output.write_bytes(&[value]).unwrap();
            assert_eq!(output.accounted_capacity, expected_capacity);
        }

        assert_eq!(output.bytes, [1, 2, 3, 4, 5]);
        assert_eq!(output.stats.wire_bytes, 5);
        assert_eq!(output.stats.owned_bytes, 8);
        let _ = output.finish();
        assert_eq!(budget.usage().bytes, 13);
    }

    #[test]
    fn traversal_metrics_and_budget_ledgers_share_one_accounting_model() {
        let mut budget = budget_with_limits(64, 8, 8, 4);
        let mut output = TypeTreeOutput::new(Endian::Little, &mut budget);
        output.enter_node(2).unwrap();
        output.enter_nodes(3, 4).unwrap();
        output.consume_members(7).unwrap();
        for value in 1_u8..=5 {
            output.write_scalar_bytes(&[value]).unwrap();
        }
        output.write_bulk_bytes(&[1, 2, 3, 4, 5, 6]).unwrap();

        assert_eq!(
            output.stats,
            TypeTreeTraversalStats {
                wire_bytes: 11,
                owned_bytes: 16,
                node_visits: 5,
                members: 7,
                bulk_runs: 1,
                bulk_bytes: 6,
                scalar_element_ops: 5,
                ..TypeTreeTraversalStats::default()
            }
        );
        let _ = output.finish();
        assert_eq!(
            budget.usage(),
            AssetLoadUsage {
                entries: 5,
                bytes: 27,
                max_observed_depth: 3,
                members: 7,
                ..AssetLoadUsage::default()
            }
        );
    }
}
