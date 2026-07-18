//! Budgeted cursor and metrics shared by TypeTree traversal adapters.

use std::hash::Hash;
use std::mem::size_of;

use indexmap::IndexMap;
use unity_asset_core::AssetLoadBudget;

use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};

/// Observable work performed by one TypeTree traversal.
///
/// Counters are monotonic. Restoring a cursor checkpoint does not subtract work because retrying
/// untrusted input must not make previously inspected bytes free.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TypeTreeTraversalStats {
    pub wire_bytes: u64,
    pub owned_bytes: u64,
    pub node_visits: u64,
    pub members: u64,
    pub bulk_runs: u64,
    pub bulk_bytes: u64,
    pub scalar_element_ops: u64,
    pub unity_values_materialized: u64,
    pub pptrs_emitted: u64,
}

/// Identifies the counter that overflowed while combining TypeTree traversal statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("TypeTree traversal statistic overflow: {field}")]
pub struct TypeTreeTraversalStatsOverflow {
    field: &'static str,
}

impl TypeTreeTraversalStatsOverflow {
    /// Returns the name of the overflowing [`TypeTreeTraversalStats`] field.
    pub const fn field(&self) -> &'static str {
        self.field
    }
}

impl TypeTreeTraversalStats {
    /// Combines two exact traversal observations, reporting the first overflowing counter.
    pub fn checked_add(
        self,
        other: Self,
    ) -> std::result::Result<Self, TypeTreeTraversalStatsOverflow> {
        macro_rules! checked {
            ($field:ident) => {
                self.$field
                    .checked_add(other.$field)
                    .ok_or(TypeTreeTraversalStatsOverflow {
                        field: stringify!($field),
                    })?
            };
        }

        Ok(Self {
            wire_bytes: checked!(wire_bytes),
            owned_bytes: checked!(owned_bytes),
            node_visits: checked!(node_visits),
            members: checked!(members),
            bulk_runs: checked!(bulk_runs),
            bulk_bytes: checked!(bulk_bytes),
            scalar_element_ops: checked!(scalar_element_ops),
            unity_values_materialized: checked!(unity_values_materialized),
            pptrs_emitted: checked!(pptrs_emitted),
        })
    }
}

/// A restorable input position.
///
/// The checkpoint deliberately contains no budget or metric state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TraversalCheckpoint {
    position: u64,
}

#[derive(Debug, Clone, Copy)]
struct PreparedAdvance {
    end: u64,
    amount: u64,
    wire_bytes: u64,
}

/// Adds caller-owned resource accounting to a bounded [`BinaryReader`].
///
/// Every operation validates its complete range and metric arithmetic before charging the budget.
/// The reader moves only after the charge succeeds. Owned reads charge both inspected wire bytes
/// and the allocation retained by the traversal.
pub(crate) struct TraversalCursor<'reader, 'data, 'budget> {
    reader: &'reader mut BinaryReader<'data>,
    budget: &'budget mut AssetLoadBudget,
    stats: TypeTreeTraversalStats,
}

impl<'reader, 'data, 'budget> TraversalCursor<'reader, 'data, 'budget> {
    pub(crate) fn new(
        reader: &'reader mut BinaryReader<'data>,
        budget: &'budget mut AssetLoadBudget,
    ) -> Result<Self> {
        if reader.position()
            > u64::try_from(reader.len()).map_err(|_| {
                BinaryError::invalid_data("TypeTree reader length does not fit in u64")
            })?
        {
            return Err(BinaryError::invalid_data(
                "TypeTree reader starts past the end of its input",
            ));
        }
        Ok(Self {
            reader,
            budget,
            stats: TypeTreeTraversalStats::default(),
        })
    }

    pub(crate) fn position(&self) -> u64 {
        self.reader.position()
    }

    pub(crate) fn checkpoint(&self) -> TraversalCheckpoint {
        TraversalCheckpoint {
            position: self.position(),
        }
    }

    /// Restores only the reader position. Budget consumption and metrics remain monotonic.
    pub(crate) fn restore(&mut self, checkpoint: TraversalCheckpoint) -> Result<()> {
        self.reader.set_position(checkpoint.position)
    }

    pub(crate) fn stats(&self) -> TypeTreeTraversalStats {
        self.stats
    }

    pub(crate) fn into_stats(self) -> TypeTreeTraversalStats {
        self.stats
    }

    pub(crate) fn read_u8(&mut self) -> Result<u8> {
        self.read_wire(1, |reader| reader.read_u8())
    }

    pub(crate) fn read_i8(&mut self) -> Result<i8> {
        self.read_wire(1, |reader| reader.read_i8())
    }

    pub(crate) fn read_u16(&mut self) -> Result<u16> {
        self.read_wire(2, |reader| reader.read_u16())
    }

    pub(crate) fn read_i16(&mut self) -> Result<i16> {
        self.read_wire(2, |reader| reader.read_i16())
    }

    pub(crate) fn read_u32(&mut self) -> Result<u32> {
        self.read_wire(4, |reader| reader.read_u32())
    }

    pub(crate) fn read_i32(&mut self) -> Result<i32> {
        self.read_wire(4, |reader| reader.read_i32())
    }

    pub(crate) fn read_u64(&mut self) -> Result<u64> {
        self.read_wire(8, |reader| reader.read_u64())
    }

    pub(crate) fn read_i64(&mut self) -> Result<i64> {
        self.read_wire(8, |reader| reader.read_i64())
    }

    pub(crate) fn read_f32(&mut self) -> Result<f32> {
        self.read_wire(4, |reader| reader.read_f32())
    }

    pub(crate) fn read_f64(&mut self) -> Result<f64> {
        self.read_wire(8, |reader| reader.read_f64())
    }

    pub(crate) fn read_bool(&mut self) -> Result<bool> {
        self.read_wire(1, |reader| reader.read_bool())
    }

    /// Reads an owned byte buffer after charging both wire and retained allocation bytes.
    pub(crate) fn read_bytes(&mut self, count: usize) -> Result<Vec<u8>> {
        let advance = self.prepare_advance(count)?;
        let owned = u64::try_from(count)
            .map_err(|_| BinaryError::memory_error("TypeTree byte length does not fit in u64"))?;
        let combined = advance.amount.checked_add(owned).ok_or_else(|| {
            BinaryError::memory_error("TypeTree wire and owned byte total overflow")
        })?;
        let owned_bytes = checked_add_metric(self.stats.owned_bytes, owned, "owned bytes")?;

        // Preflight the combined charge so an owned-budget failure cannot occur after the wire
        // portion has already been charged.
        self.budget.check_bytes(combined)?;

        let mut bytes = Vec::new();
        bytes.try_reserve_exact(count).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {count} bytes for TypeTree traversal: {error}"
            ))
        })?;

        let source = self
            .reader
            .remaining_slice()
            .get(..count)
            .ok_or_else(|| BinaryError::not_enough_data(count, self.reader.remaining()))?;
        bytes.extend_from_slice(source);

        // The preflight above makes this charge infallible unless the budget implementation is
        // internally inconsistent. Allocation failure leaves both ledgers untouched.
        self.budget.consume_bytes(combined)?;
        self.reader.set_position(advance.end)?;

        self.stats.wire_bytes = advance.wire_bytes;
        self.stats.owned_bytes = owned_bytes;
        Ok(bytes)
    }

    /// Inspects one wire extent without copying it or classifying it as a primitive bulk run.
    ///
    /// The wire extent is charged and consumed before an inspection error is returned, matching
    /// owned reads whose decoding can fail only after the payload has been read.
    pub(crate) fn with_borrowed_slice<T>(
        &mut self,
        count: usize,
        inspect: impl FnOnce(&[u8]) -> Result<T>,
    ) -> Result<T> {
        let advance = self.prepare_advance(count)?;
        self.budget.check_bytes(advance.amount)?;

        let source = self
            .reader
            .remaining_slice()
            .get(..count)
            .ok_or_else(|| BinaryError::not_enough_data(count, self.reader.remaining()))?;
        let inspected = inspect(source);

        self.budget.consume_bytes(advance.amount)?;
        self.reader.set_position(advance.end)?;
        self.stats.wire_bytes = advance.wire_bytes;
        inspected
    }

    /// Decodes one contiguous wire run without copying it into an intermediate byte buffer.
    ///
    /// The decoder is transactional with respect to cursor movement, budget usage, and metrics:
    /// all three advance only after the closure succeeds.
    pub(crate) fn with_wire_slice<T>(
        &mut self,
        count: usize,
        decode: impl FnOnce(&[u8], ByteOrder) -> Result<T>,
    ) -> Result<T> {
        let advance = self.prepare_advance(count)?;
        let bulk_runs = checked_add_metric(self.stats.bulk_runs, 1, "bulk runs")?;
        let bulk_bytes = checked_add_metric(self.stats.bulk_bytes, advance.amount, "bulk bytes")?;
        self.budget.check_bytes(advance.amount)?;

        let source = self
            .reader
            .remaining_slice()
            .get(..count)
            .ok_or_else(|| BinaryError::not_enough_data(count, self.reader.remaining()))?;
        let value = decode(source, self.reader.byte_order())?;

        self.budget.consume_bytes(advance.amount)?;
        self.reader.set_position(advance.end)?;
        self.stats.wire_bytes = advance.wire_bytes;
        self.stats.bulk_runs = bulk_runs;
        self.stats.bulk_bytes = bulk_bytes;
        Ok(value)
    }

    /// Advances without allocating while charging every skipped wire byte.
    pub(crate) fn skip_bytes(&mut self, count: usize) -> Result<()> {
        let advance = self.prepare_advance(count)?;
        self.budget.consume_bytes(advance.amount)?;
        self.reader.set_position(advance.end)?;
        self.stats.wire_bytes = advance.wire_bytes;
        Ok(())
    }

    pub(crate) fn align(&mut self) -> Result<()> {
        self.align_to(4)
    }

    /// Moves to the next absolute alignment boundary and charges the exact padding extent.
    pub(crate) fn align_to(&mut self, alignment: u64) -> Result<()> {
        if alignment == 0 {
            return Err(BinaryError::invalid_data(
                "TypeTree traversal alignment must be nonzero",
            ));
        }
        let position = self.position();
        let remainder = position % alignment;
        let padding = if remainder == 0 {
            0
        } else {
            alignment - remainder
        };
        let padding = usize::try_from(padding).map_err(|_| {
            BinaryError::invalid_data("TypeTree alignment padding does not fit usize")
        })?;
        self.skip_bytes(padding)
    }

    /// Accounts for entering a canonical schema node before recursive traversal.
    pub(crate) fn enter_node(&mut self, depth: u32) -> Result<()> {
        self.enter_nodes(depth, 1)
    }

    /// Accounts for a repeated canonical node without forcing a scalar loop.
    pub(crate) fn enter_nodes(&mut self, depth: u32, amount: u64) -> Result<()> {
        let node_visits = checked_add_metric(self.stats.node_visits, amount, "node visits")?;
        self.budget.check_depth(depth)?;
        self.budget.check_entries(amount)?;
        self.budget.consume_entries(amount)?;
        self.budget.observe_depth(depth)?;
        self.stats.node_visits = node_visits;
        Ok(())
    }

    /// Accounts for a dynamic collection before reserving or traversing its elements.
    pub(crate) fn consume_members(&mut self, amount: u64) -> Result<()> {
        let members = checked_add_metric(self.stats.members, amount, "members")?;
        self.budget.check_members(amount)?;
        self.budget.consume_members(amount)?;
        self.stats.members = members;
        Ok(())
    }

    /// Creates a vector whose usable capacity is owned by this budget ledger.
    pub(crate) fn vector<T>(
        &mut self,
        capacity: usize,
        label: &'static str,
    ) -> Result<TraversalVec<T>> {
        TraversalVec::with_capacity(self, capacity, label)
    }

    /// Creates an ordered map whose usable capacity is owned by this budget ledger.
    pub(crate) fn map<K: Eq + Hash, V>(
        &mut self,
        capacity: usize,
        label: &'static str,
    ) -> Result<TraversalMap<K, V>> {
        TraversalMap::with_capacity(self, capacity, label)
    }

    /// Reserves UTF-8 storage through the owned-byte budget.
    pub(crate) fn reserve_string(&mut self, value: &mut String, additional: usize) -> Result<()> {
        let required = value.len().checked_add(additional).ok_or_else(|| {
            BinaryError::memory_error("TypeTree string capacity arithmetic overflow")
        })?;
        if required <= value.capacity() {
            return Ok(());
        }
        let additional_capacity = required - value.capacity();
        let allocation = u64::try_from(additional_capacity).map_err(|_| {
            BinaryError::memory_error("TypeTree string capacity does not fit in u64")
        })?;
        let owned_bytes = checked_add_metric(self.stats.owned_bytes, allocation, "owned bytes")?;
        self.budget.check_bytes(allocation)?;
        value.try_reserve_exact(additional).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {additional} bytes for TypeTree string: {error}"
            ))
        })?;
        self.budget.consume_bytes(allocation)?;
        self.stats.owned_bytes = owned_bytes;
        Ok(())
    }

    /// Clones output text through the owned-byte ledger.
    pub(crate) fn clone_string(&mut self, value: &str, label: &str) -> Result<String> {
        let mut owned = String::new();
        self.reserve_string(&mut owned, value.len())
            .map_err(|error| {
                BinaryError::memory_error(format!("Failed to reserve {label}: {error}"))
            })?;
        owned.push_str(value);
        Ok(owned)
    }

    fn reserve_map<K, V>(
        &mut self,
        values: &mut IndexMap<K, V>,
        accounted_capacity: &mut usize,
        additional: usize,
        label: &str,
    ) -> Result<()> {
        let required = values.len().checked_add(additional).ok_or_else(|| {
            BinaryError::memory_error(format!("{label} capacity arithmetic overflow"))
        })?;
        if required <= *accounted_capacity {
            return Ok(());
        }

        let additional_capacity = required - *accounted_capacity;
        let slot_width = size_of::<K>()
            .checked_add(size_of::<V>())
            .and_then(|width| width.checked_add(size_of::<usize>() * 2))
            .ok_or_else(|| BinaryError::memory_error(format!("{label} slot size overflow")))?;
        let allocation = additional_capacity.checked_mul(slot_width).ok_or_else(|| {
            BinaryError::memory_error(format!("{label} allocation size overflow"))
        })?;
        let allocation = u64::try_from(allocation).map_err(|_| {
            BinaryError::memory_error(format!("{label} allocation does not fit in u64"))
        })?;
        let owned_bytes = checked_add_metric(self.stats.owned_bytes, allocation, "owned bytes")?;
        self.budget.check_bytes(allocation)?;
        values.try_reserve(additional).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {additional} entries for {label}: {error}"
            ))
        })?;
        self.budget.consume_bytes(allocation)?;
        self.stats.owned_bytes = owned_bytes;
        *accounted_capacity = required;
        Ok(())
    }

    pub(crate) fn record_scalar_elements(&mut self, amount: u64) -> Result<()> {
        self.stats.scalar_element_ops = checked_add_metric(
            self.stats.scalar_element_ops,
            amount,
            "scalar element operations",
        )?;
        Ok(())
    }

    pub(crate) fn record_materialized(&mut self, amount: u64) -> Result<()> {
        self.stats.unity_values_materialized = checked_add_metric(
            self.stats.unity_values_materialized,
            amount,
            "materialized Unity values",
        )?;
        Ok(())
    }

    pub(crate) fn record_pptrs(&mut self, amount: u64) -> Result<()> {
        self.stats.pptrs_emitted =
            checked_add_metric(self.stats.pptrs_emitted, amount, "emitted PPtrs")?;
        Ok(())
    }

    fn read_wire<T>(
        &mut self,
        count: usize,
        read: impl FnOnce(&mut BinaryReader<'data>) -> Result<T>,
    ) -> Result<T> {
        let advance = self.prepare_advance(count)?;
        self.budget.consume_bytes(advance.amount)?;
        let value = read(self.reader)?;
        if self.reader.position() != advance.end {
            return Err(BinaryError::invalid_data(
                "TypeTree primitive reader consumed an unexpected extent",
            ));
        }
        self.stats.wire_bytes = advance.wire_bytes;
        Ok(value)
    }

    fn prepare_advance(&self, count: usize) -> Result<PreparedAdvance> {
        let remaining = self.reader.remaining();
        if count > remaining {
            return Err(BinaryError::not_enough_data(count, remaining));
        }
        let amount = u64::try_from(count)
            .map_err(|_| BinaryError::invalid_data("TypeTree extent does not fit in u64"))?;
        let end = self
            .position()
            .checked_add(amount)
            .ok_or_else(|| BinaryError::invalid_data("TypeTree cursor position overflow"))?;
        let wire_bytes = checked_add_metric(self.stats.wire_bytes, amount, "wire bytes")?;
        Ok(PreparedAdvance {
            end,
            amount,
            wire_bytes,
        })
    }

    fn reserve_vec<T>(
        &mut self,
        values: &mut Vec<T>,
        accounted_capacity: &mut usize,
        additional: usize,
        label: &str,
    ) -> Result<()> {
        let required = values.len().checked_add(additional).ok_or_else(|| {
            BinaryError::memory_error(format!("{label} capacity arithmetic overflow"))
        })?;
        if required <= *accounted_capacity {
            return Ok(());
        }
        let target_capacity = if *accounted_capacity == 0 {
            required
        } else {
            accounted_capacity
                .checked_mul(2)
                .ok_or_else(|| {
                    BinaryError::memory_error(format!("{label} geometric capacity overflow"))
                })?
                .max(required)
        };
        let additional_capacity = target_capacity - *accounted_capacity;
        let allocation = additional_capacity
            .checked_mul(size_of::<T>())
            .ok_or_else(|| {
                BinaryError::memory_error(format!("{label} allocation size overflow"))
            })?;
        let allocation = u64::try_from(allocation).map_err(|_| {
            BinaryError::memory_error(format!("{label} allocation does not fit in u64"))
        })?;
        let owned_bytes = checked_add_metric(self.stats.owned_bytes, allocation, "owned bytes")?;
        self.budget.check_bytes(allocation)?;
        let reserve = target_capacity - values.len();
        values.try_reserve_exact(reserve).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {reserve} entries for {label}: {error}"
            ))
        })?;
        self.budget.consume_bytes(allocation)?;
        self.stats.owned_bytes = owned_bytes;
        *accounted_capacity = target_capacity;
        Ok(())
    }
}

/// A vector whose logical capacity has been charged to a [`TraversalCursor`].
pub(crate) struct TraversalVec<T> {
    values: Vec<T>,
    accounted_capacity: usize,
    label: &'static str,
}

impl<T> TraversalVec<T> {
    fn with_capacity(
        cursor: &mut TraversalCursor<'_, '_, '_>,
        capacity: usize,
        label: &'static str,
    ) -> Result<Self> {
        let mut values = Vec::new();
        let mut accounted_capacity = 0;
        cursor.reserve_vec(&mut values, &mut accounted_capacity, capacity, label)?;
        Ok(Self {
            values,
            accounted_capacity,
            label,
        })
    }

    pub(crate) fn push(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        value: T,
    ) -> Result<()> {
        cursor.reserve_vec(
            &mut self.values,
            &mut self.accounted_capacity,
            1,
            self.label,
        )?;
        self.values.push(value);
        Ok(())
    }

    pub(crate) fn into_vec(self) -> Vec<T> {
        self.values
    }
}

/// An insertion-ordered map whose logical capacity has been charged to a traversal budget.
pub(crate) struct TraversalMap<K, V> {
    values: IndexMap<K, V>,
    accounted_capacity: usize,
    label: &'static str,
}

impl<K, V> TraversalMap<K, V>
where
    K: Eq + Hash,
{
    fn with_capacity(
        cursor: &mut TraversalCursor<'_, '_, '_>,
        capacity: usize,
        label: &'static str,
    ) -> Result<Self> {
        let mut values = IndexMap::new();
        let mut accounted_capacity = 0;
        cursor.reserve_map(&mut values, &mut accounted_capacity, capacity, label)?;
        Ok(Self {
            values,
            accounted_capacity,
            label,
        })
    }

    pub(crate) fn insert(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        key: K,
        value: V,
    ) -> Result<Option<V>> {
        if !self.values.contains_key(&key) {
            cursor.reserve_map(
                &mut self.values,
                &mut self.accounted_capacity,
                1,
                self.label,
            )?;
        }
        Ok(self.values.insert(key, value))
    }

    pub(crate) fn into_map(self) -> IndexMap<K, V> {
        self.values
    }
}

fn checked_add_metric(current: u64, amount: u64, resource: &str) -> Result<u64> {
    current.checked_add(amount).ok_or_else(|| {
        BinaryError::invalid_data(format!("TypeTree traversal {resource} counter overflow"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::AssetLoadLimits;

    fn budget_with_bytes(max_bytes: u64) -> AssetLoadBudget {
        AssetLoadBudget::new(AssetLoadLimits {
            max_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap()
    }

    #[test]
    fn checked_stats_add_combines_every_counter() {
        let left = TypeTreeTraversalStats {
            wire_bytes: 1,
            owned_bytes: 2,
            node_visits: 3,
            members: 4,
            bulk_runs: 5,
            bulk_bytes: 6,
            scalar_element_ops: 7,
            unity_values_materialized: 8,
            pptrs_emitted: 9,
        };
        let right = TypeTreeTraversalStats {
            wire_bytes: 10,
            owned_bytes: 20,
            node_visits: 30,
            members: 40,
            bulk_runs: 50,
            bulk_bytes: 60,
            scalar_element_ops: 70,
            unity_values_materialized: 80,
            pptrs_emitted: 90,
        };

        assert_eq!(
            left.checked_add(right).unwrap(),
            TypeTreeTraversalStats {
                wire_bytes: 11,
                owned_bytes: 22,
                node_visits: 33,
                members: 44,
                bulk_runs: 55,
                bulk_bytes: 66,
                scalar_element_ops: 77,
                unity_values_materialized: 88,
                pptrs_emitted: 99,
            }
        );
    }

    #[test]
    fn checked_stats_add_reports_each_overflowing_counter() {
        let cases = [
            (
                "wire_bytes",
                TypeTreeTraversalStats {
                    wire_bytes: u64::MAX,
                    ..TypeTreeTraversalStats::default()
                },
            ),
            (
                "owned_bytes",
                TypeTreeTraversalStats {
                    owned_bytes: u64::MAX,
                    ..TypeTreeTraversalStats::default()
                },
            ),
            (
                "node_visits",
                TypeTreeTraversalStats {
                    node_visits: u64::MAX,
                    ..TypeTreeTraversalStats::default()
                },
            ),
            (
                "members",
                TypeTreeTraversalStats {
                    members: u64::MAX,
                    ..TypeTreeTraversalStats::default()
                },
            ),
            (
                "bulk_runs",
                TypeTreeTraversalStats {
                    bulk_runs: u64::MAX,
                    ..TypeTreeTraversalStats::default()
                },
            ),
            (
                "bulk_bytes",
                TypeTreeTraversalStats {
                    bulk_bytes: u64::MAX,
                    ..TypeTreeTraversalStats::default()
                },
            ),
            (
                "scalar_element_ops",
                TypeTreeTraversalStats {
                    scalar_element_ops: u64::MAX,
                    ..TypeTreeTraversalStats::default()
                },
            ),
            (
                "unity_values_materialized",
                TypeTreeTraversalStats {
                    unity_values_materialized: u64::MAX,
                    ..TypeTreeTraversalStats::default()
                },
            ),
            (
                "pptrs_emitted",
                TypeTreeTraversalStats {
                    pptrs_emitted: u64::MAX,
                    ..TypeTreeTraversalStats::default()
                },
            ),
        ];
        let one = TypeTreeTraversalStats {
            wire_bytes: 1,
            owned_bytes: 1,
            node_visits: 1,
            members: 1,
            bulk_runs: 1,
            bulk_bytes: 1,
            scalar_element_ops: 1,
            unity_values_materialized: 1,
            pptrs_emitted: 1,
        };

        for (field, left) in cases {
            let error = left.checked_add(one).unwrap_err();
            assert_eq!(error.field(), field);
            assert_eq!(
                error.to_string(),
                format!("TypeTree traversal statistic overflow: {field}")
            );
        }
    }

    #[test]
    fn primitive_skip_and_alignment_charge_exact_wire_extent() {
        let data = [1_u8, 0, 0, 0, 2, 3, 4, 5];
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);
        let mut budget = budget_with_bytes(64);
        let mut cursor = TraversalCursor::new(&mut reader, &mut budget).unwrap();

        assert_eq!(cursor.read_u8().unwrap(), 1);
        cursor.align().unwrap();
        cursor.skip_bytes(2).unwrap();
        assert_eq!(cursor.position(), 6);
        assert_eq!(cursor.stats().wire_bytes, 6);

        let stats = cursor.into_stats();
        assert_eq!(stats.owned_bytes, 0);
        assert_eq!(budget.usage().bytes, 6);
    }

    #[test]
    fn owned_read_charges_wire_and_retained_bytes_before_moving() {
        let data = [1_u8, 2, 3, 4];
        let mut reader = BinaryReader::new(&data, ByteOrder::Big);
        let mut budget = budget_with_bytes(8);
        let mut cursor = TraversalCursor::new(&mut reader, &mut budget).unwrap();

        assert_eq!(cursor.read_bytes(4).unwrap(), data);
        let stats = cursor.into_stats();

        assert_eq!(stats.wire_bytes, 4);
        assert_eq!(stats.owned_bytes, 4);
        assert_eq!(budget.usage().bytes, 8);
    }

    #[test]
    fn owned_budget_failure_leaves_reader_and_stats_unchanged() {
        let data = [1_u8, 2];
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);
        let mut budget = budget_with_bytes(3);
        let mut cursor = TraversalCursor::new(&mut reader, &mut budget).unwrap();

        assert!(cursor.read_bytes(2).is_err());
        assert_eq!(cursor.position(), 0);
        assert_eq!(cursor.stats(), TypeTreeTraversalStats::default());

        cursor.into_stats();
        assert_eq!(budget.usage().bytes, 0);
    }

    #[test]
    fn out_of_range_skip_does_not_charge_or_move() {
        let data = [1_u8, 2];
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);
        let mut budget = budget_with_bytes(8);
        let mut cursor = TraversalCursor::new(&mut reader, &mut budget).unwrap();

        let error = cursor.skip_bytes(3).unwrap_err();
        assert!(matches!(error, BinaryError::NotEnoughData { .. }));
        assert_eq!(cursor.position(), 0);
        assert_eq!(cursor.stats().wire_bytes, 0);

        cursor.into_stats();
        assert_eq!(budget.usage().bytes, 0);
    }

    #[test]
    fn restoring_checkpoint_does_not_refund_work() {
        let data = [7_u8, 8];
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);
        let mut budget = budget_with_bytes(8);
        let mut cursor = TraversalCursor::new(&mut reader, &mut budget).unwrap();
        let checkpoint = cursor.checkpoint();

        assert_eq!(cursor.read_u8().unwrap(), 7);
        cursor.restore(checkpoint).unwrap();
        assert_eq!(cursor.read_u8().unwrap(), 7);
        assert_eq!(cursor.position(), 1);

        let stats = cursor.into_stats();
        assert_eq!(stats.wire_bytes, 2);
        assert_eq!(budget.usage().bytes, 2);
    }

    #[test]
    fn structure_and_adapter_metrics_are_monotonic() {
        let data = [0_u8; 32];
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);
        let mut budget = budget_with_bytes(128);
        let mut cursor = TraversalCursor::new(&mut reader, &mut budget).unwrap();

        cursor.enter_node(3).unwrap();
        cursor.consume_members(7).unwrap();
        cursor
            .with_wire_slice(32, |bytes, _| {
                assert_eq!(bytes.len(), 32);
                Ok(())
            })
            .unwrap();
        cursor.record_scalar_elements(5).unwrap();
        cursor.record_materialized(9).unwrap();
        cursor.record_pptrs(2).unwrap();
        let stats = cursor.into_stats();

        assert_eq!(stats.node_visits, 1);
        assert_eq!(stats.members, 7);
        assert_eq!(stats.bulk_runs, 1);
        assert_eq!(stats.bulk_bytes, 32);
        assert_eq!(stats.scalar_element_ops, 5);
        assert_eq!(stats.unity_values_materialized, 9);
        assert_eq!(stats.pptrs_emitted, 2);
        assert_eq!(budget.usage().entries, 1);
        assert_eq!(budget.usage().members, 7);
        assert_eq!(budget.usage().max_observed_depth, 3);
    }

    #[test]
    fn reserve_helpers_charge_retained_capacity_before_allocation() {
        let data = [];
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);
        let mut budget = budget_with_bytes(1_024);
        let mut cursor = TraversalCursor::new(&mut reader, &mut budget).unwrap();
        let mut values = cursor.vector::<u64>(1, "test values").unwrap();
        values.push(&mut cursor, 1).unwrap();
        values.push(&mut cursor, 2).unwrap();
        let strings = cursor.vector::<String>(1, "test strings").unwrap();
        let bytes = cursor.vector::<u8>(3, "test bytes").unwrap();
        let mut map = cursor.map::<String, u64>(1, "test map").unwrap();
        map.insert(&mut cursor, "answer".to_string(), 42).unwrap();
        let string = cursor.clone_string("test", "test string").unwrap();

        assert_eq!(string, "test");
        let values = values.into_vec();
        let strings = strings.into_vec();
        let bytes = bytes.into_vec();
        let map = map.into_map();
        assert_eq!(values, vec![1, 2]);
        assert!(strings.is_empty());
        assert!(bytes.is_empty());
        assert_eq!(map.get("answer"), Some(&42));
        let stats = cursor.into_stats();
        let map_slot = size_of::<String>() + size_of::<u64>() + 2 * size_of::<usize>();
        let expected = (2 * size_of::<u64>() + size_of::<String>() + 3 + map_slot + 4) as u64;

        assert_eq!(stats.owned_bytes, expected);
        assert_eq!(budget.usage().bytes, expected);
    }

    #[test]
    fn dynamic_vectors_grow_geometrically_under_the_budget() {
        let data = [];
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);
        let mut budget = budget_with_bytes(1_024);
        let mut cursor = TraversalCursor::new(&mut reader, &mut budget).unwrap();
        let mut values = cursor.vector::<u64>(0, "growing values").unwrap();

        for value in 0..5 {
            values.push(&mut cursor, value).unwrap();
        }

        assert_eq!(values.accounted_capacity, 8);
        assert!(values.values.capacity() >= 8);
        assert_eq!(values.into_vec(), vec![0, 1, 2, 3, 4]);
        let stats = cursor.into_stats();
        assert_eq!(stats.owned_bytes, u64::from(u64::BITS));
        assert_eq!(budget.usage().bytes, stats.owned_bytes);
    }

    #[test]
    fn metric_overflow_is_rejected_before_wire_charge_or_movement() {
        let data = [1_u8];
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);
        let mut budget = budget_with_bytes(8);
        let mut cursor = TraversalCursor::new(&mut reader, &mut budget).unwrap();
        cursor.stats.wire_bytes = u64::MAX;

        assert!(cursor.read_u8().is_err());
        assert_eq!(cursor.position(), 0);

        cursor.into_stats();
        assert_eq!(budget.usage().bytes, 0);
    }

    #[test]
    fn bulk_decode_failure_is_transactional() {
        let data = [1_u8, 2, 3, 4];
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);
        let mut budget = budget_with_bytes(8);
        let mut cursor = TraversalCursor::new(&mut reader, &mut budget).unwrap();

        let result = cursor.with_wire_slice(4, |_, _| -> Result<()> {
            Err(BinaryError::invalid_data("decoder rejected run"))
        });

        assert!(result.is_err());
        assert_eq!(cursor.position(), 0);
        assert_eq!(cursor.stats(), TypeTreeTraversalStats::default());
        cursor.into_stats();
        assert_eq!(budget.usage().bytes, 0);
    }

    #[test]
    fn cursor_rejects_reader_position_past_input() {
        let data = [1_u8];
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);
        reader.seek(2).unwrap();
        let mut budget = budget_with_bytes(8);

        assert!(TraversalCursor::new(&mut reader, &mut budget).is_err());
        assert_eq!(budget.usage().bytes, 0);
    }
}
