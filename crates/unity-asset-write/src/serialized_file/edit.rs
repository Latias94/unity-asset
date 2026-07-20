use std::collections::TryReserveError;

use thiserror::Error;
use unity_asset_binary::asset::FileIdentifier;
use unity_asset_core::{AllocationSizeError, AssetLoadBudget, BudgetError, vec_allocation_bytes};

use super::external_table::ExternalTableMutation;

const OBJECT_EDIT_ALLOCATION_RESOURCE: &str = "serialized_file_object_edits";

/// Typed failures produced while retaining a SerializedFile object replacement.
#[derive(Debug, Error)]
pub enum SerializedFileEditError {
    #[error("failed to reserve {requested} SerializedFile object edits: {source}")]
    Allocation {
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error(transparent)]
    AllocationSize(#[from] AllocationSizeError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("SerializedFile object edit arithmetic overflow for {resource}")]
    ArithmeticOverflow { resource: &'static str },
}

#[derive(Debug)]
struct ObjectBytesEdit {
    path_id: i64,
    bytes: Vec<u8>,
}

/// Deterministic, budgeted edits to apply when rebuilding one `SerializedFile`.
///
/// Object replacements are retained in ascending path-ID order. The representation is private so
/// callers cannot bypass allocation accounting or create duplicate replacements for one object.
#[derive(Debug, Default)]
pub struct SerializedFileEdits {
    object_bytes: Vec<ObjectBytesEdit>,
    pub(crate) external_table: Option<ExternalTableMutation>,
}

impl SerializedFileEdits {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            object_bytes: Vec::new(),
            external_table: None,
        }
    }

    /// Inserts or replaces raw object bytes while atomically charging retained storage.
    ///
    /// The incoming byte allocation is charged by capacity because the edit set assumes ownership
    /// of the complete allocation. A failed budget check or table allocation leaves both the edit
    /// set and budget usage unchanged.
    pub fn try_set_object_bytes(
        &mut self,
        path_id: i64,
        bytes: Vec<u8>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), SerializedFileEditError> {
        let byte_allocation = vec_allocation_bytes::<u8>(bytes.capacity())?;
        match self
            .object_bytes
            .binary_search_by_key(&path_id, |edit| edit.path_id)
        {
            Ok(index) => {
                budget.check_bytes(byte_allocation)?;
                budget.consume_bytes(byte_allocation)?;
                self.object_bytes[index].bytes = bytes;
                Ok(())
            }
            Err(index) => self.insert_object_bytes(index, path_id, bytes, byte_allocation, budget),
        }
    }

    /// Returns a retained replacement for `path_id`.
    #[must_use]
    pub fn object_bytes(&self, path_id: i64) -> Option<&[u8]> {
        self.object_bytes
            .binary_search_by_key(&path_id, |edit| edit.path_id)
            .ok()
            .map(|index| self.object_bytes[index].bytes.as_slice())
    }

    /// Consumes the complete edit set and moves out the replacement for `path_id`.
    ///
    /// This is the zero-copy handoff for a caller that used an edit set as a temporary encoding
    /// transaction. Every other replacement and external-table addition is discarded with `self`.
    #[must_use]
    pub fn into_object_bytes(mut self, path_id: i64) -> Option<Vec<u8>> {
        let index = self
            .object_bytes
            .binary_search_by_key(&path_id, |edit| edit.path_id)
            .ok()?;
        Some(self.object_bytes.swap_remove(index).bytes)
    }

    /// Iterates edited path IDs in their deterministic ascending order.
    pub fn object_path_ids(&self) -> impl ExactSizeIterator<Item = i64> + DoubleEndedIterator + '_ {
        self.object_bytes.iter().map(|edit| edit.path_id)
    }

    /// Returns validated external identifiers appended by an external-table allocator.
    #[must_use]
    pub fn external_additions(&self) -> &[FileIdentifier] {
        self.external_table
            .as_ref()
            .map_or(&[], ExternalTableMutation::additions)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.object_bytes.is_empty() && self.external_additions().is_empty()
    }

    fn insert_object_bytes(
        &mut self,
        index: usize,
        path_id: i64,
        bytes: Vec<u8>,
        byte_allocation: u64,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), SerializedFileEditError> {
        budget.check_entries(1)?;
        if self.object_bytes.len() < self.object_bytes.capacity() {
            budget.check_bytes(byte_allocation)?;
            budget.consume_entries(1)?;
            budget.consume_bytes(byte_allocation)?;
            self.object_bytes
                .insert(index, ObjectBytesEdit { path_id, bytes });
            return Ok(());
        }

        let required = self.object_bytes.len().checked_add(1).ok_or(
            SerializedFileEditError::ArithmeticOverflow {
                resource: "object edit count",
            },
        )?;
        let planned_table = vec_allocation_bytes::<ObjectBytesEdit>(required)?;
        let planned = byte_allocation.checked_add(planned_table).ok_or(
            SerializedFileEditError::ArithmeticOverflow {
                resource: OBJECT_EDIT_ALLOCATION_RESOURCE,
            },
        )?;
        budget.check_bytes(planned)?;

        let mut staged = Vec::new();
        staged.try_reserve_exact(required).map_err(|source| {
            SerializedFileEditError::Allocation {
                requested: required,
                source,
            }
        })?;
        let actual_table = vec_allocation_bytes::<ObjectBytesEdit>(staged.capacity())?;
        let actual = byte_allocation.checked_add(actual_table).ok_or(
            SerializedFileEditError::ArithmeticOverflow {
                resource: OBJECT_EDIT_ALLOCATION_RESOURCE,
            },
        )?;
        budget.check_bytes(actual)?;
        budget.consume_entries(1)?;
        budget.consume_bytes(actual)?;

        staged.append(&mut self.object_bytes);
        staged.insert(index, ObjectBytesEdit { path_id, bytes });
        self.object_bytes = staged;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use unity_asset_core::{AssetLoadLimits, AssetLoadUsage};

    use super::*;

    #[test]
    fn object_replacements_are_unique_and_sorted_by_signed_path_id() {
        let mut edits = SerializedFileEdits::new();
        let mut budget = AssetLoadBudget::default();

        edits.try_set_object_bytes(9, vec![9], &mut budget).unwrap();
        edits
            .try_set_object_bytes(-4, vec![4], &mut budget)
            .unwrap();
        edits.try_set_object_bytes(2, vec![2], &mut budget).unwrap();
        edits
            .try_set_object_bytes(2, vec![20], &mut budget)
            .unwrap();

        assert_eq!(edits.object_path_ids().collect::<Vec<_>>(), [-4, 2, 9]);
        assert_eq!(edits.object_bytes(2), Some([20].as_slice()));
    }

    #[test]
    fn consuming_lookup_moves_the_original_byte_allocation() {
        let mut replacement = Vec::with_capacity(32);
        replacement.extend_from_slice(&[1, 2, 3]);
        let allocation = replacement.as_ptr();
        let capacity = replacement.capacity();
        let mut edits = SerializedFileEdits::new();
        edits
            .try_set_object_bytes(7, replacement, &mut AssetLoadBudget::default())
            .unwrap();
        edits
            .try_set_object_bytes(-2, vec![9], &mut AssetLoadBudget::default())
            .unwrap();

        let replacement = edits.into_object_bytes(7).unwrap();

        assert_eq!(replacement.as_ptr(), allocation);
        assert_eq!(replacement.capacity(), capacity);
        assert_eq!(replacement, [1, 2, 3]);
    }

    #[test]
    fn failed_insert_leaves_edits_and_budget_unchanged() {
        let mut measured = AssetLoadBudget::default();
        let mut measured_edits = SerializedFileEdits::new();
        measured_edits
            .try_set_object_bytes(7, vec![1, 2, 3], &mut measured)
            .unwrap();
        let required = measured.usage().bytes;
        assert!(required > 0);

        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: required - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let mut edits = SerializedFileEdits::new();
        let error = edits
            .try_set_object_bytes(7, vec![1, 2, 3], &mut budget)
            .unwrap_err();

        assert!(matches!(
            error,
            SerializedFileEditError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert!(edits.is_empty());
        assert_eq!(budget.usage(), AssetLoadUsage::default());
    }

    #[test]
    fn failed_growth_preserves_the_existing_sorted_prefix() {
        let mut measured_edits = SerializedFileEdits::new();
        measured_edits
            .try_set_object_bytes(7, vec![7], &mut AssetLoadBudget::default())
            .unwrap();
        let mut measured = AssetLoadBudget::default();
        measured_edits
            .try_set_object_bytes(-2, vec![2], &mut measured)
            .unwrap();
        let required = measured.usage().bytes;
        assert!(required > 0);

        let mut edits = SerializedFileEdits::new();
        edits
            .try_set_object_bytes(7, vec![7], &mut AssetLoadBudget::default())
            .unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: required - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = edits
            .try_set_object_bytes(-2, vec![2], &mut budget)
            .unwrap_err();

        assert!(matches!(
            error,
            SerializedFileEditError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert_eq!(edits.object_path_ids().collect::<Vec<_>>(), [7]);
        assert_eq!(edits.object_bytes(7), Some([7].as_slice()));
        assert_eq!(budget.usage(), AssetLoadUsage::default());
    }

    #[test]
    fn failed_replacement_preserves_prior_bytes_and_budget() {
        let mut edits = SerializedFileEdits::new();
        edits
            .try_set_object_bytes(7, vec![1, 2, 3], &mut AssetLoadBudget::default())
            .unwrap();

        let mut replacement = Vec::with_capacity(32);
        replacement.extend_from_slice(&[4, 5, 6]);
        let replacement_capacity = replacement.capacity() as u64;
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: replacement_capacity - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = edits
            .try_set_object_bytes(7, replacement, &mut budget)
            .unwrap_err();

        assert!(matches!(
            error,
            SerializedFileEditError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert_eq!(edits.object_bytes(7), Some([1, 2, 3].as_slice()));
        assert_eq!(budget.usage(), AssetLoadUsage::default());
    }

    #[test]
    fn successful_replacement_charges_owned_capacity_without_new_entry() {
        let mut edits = SerializedFileEdits::new();
        edits
            .try_set_object_bytes(7, vec![1], &mut AssetLoadBudget::default())
            .unwrap();

        let mut replacement = Vec::with_capacity(32);
        replacement.push(9);
        let replacement_capacity = replacement.capacity() as u64;
        let mut budget = AssetLoadBudget::default();
        edits
            .try_set_object_bytes(7, replacement, &mut budget)
            .unwrap();

        assert_eq!(budget.usage().bytes, replacement_capacity);
        assert_eq!(budget.usage().entries, 0);
        assert_eq!(edits.object_bytes(7), Some([9].as_slice()));
    }
}
