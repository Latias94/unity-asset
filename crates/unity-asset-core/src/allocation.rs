//! Conservative accounting for Rust-owned heap allocations.
//!
//! The estimates cover the layout bytes requested from the global allocator, including the
//! project-visible control blocks and alignment padding. Allocator size-class rounding, allocator
//! metadata, guard pages, and process RSS are intentionally outside this accounting domain.

use std::mem::{align_of, size_of};

use thiserror::Error;

/// Arithmetic failure while estimating a Rust-owned heap allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("{allocation} allocation size cannot be represented by the allocation accounting domain")]
pub struct AllocationSizeError {
    allocation: &'static str,
}

impl AllocationSizeError {
    const fn new(allocation: &'static str) -> Self {
        Self { allocation }
    }

    #[must_use]
    pub const fn allocation(self) -> &'static str {
        self.allocation
    }
}

/// Returns the requested backing bytes for a `Vec<T>` with `capacity` slots.
pub fn vec_allocation_bytes<T>(capacity: usize) -> Result<u64, AllocationSizeError> {
    let bytes = size_of::<T>()
        .checked_mul(capacity)
        .ok_or_else(|| AllocationSizeError::new("Vec"))?;
    usize_to_u64(bytes, "Vec")
}

/// Returns the requested backing bytes for a `String` with the given capacity.
pub fn string_allocation_bytes(capacity: usize) -> Result<u64, AllocationSizeError> {
    usize_to_u64(capacity, "String")
}

/// Conservatively estimates the control block and value allocation retained by `Arc<T>`.
pub fn arc_value_allocation_bytes<T>() -> Result<u64, AllocationSizeError> {
    arc_allocation_bytes(size_of::<T>(), align_of::<T>(), "Arc value")
}

/// Conservatively estimates the single allocation retained by `Arc<[T]>`.
pub fn arc_slice_allocation_bytes<T>(length: usize) -> Result<u64, AllocationSizeError> {
    let payload_bytes = size_of::<T>()
        .checked_mul(length)
        .ok_or_else(|| AllocationSizeError::new("Arc slice"))?;
    arc_allocation_bytes(payload_bytes, align_of::<T>(), "Arc slice")
}

/// Conservatively estimates both allocations retained by `Arc<Vec<T>>`.
pub fn arc_vec_allocation_bytes<T>(capacity: usize) -> Result<u64, AllocationSizeError> {
    let control = arc_value_allocation_bytes::<Vec<T>>()?;
    let backing = vec_allocation_bytes::<T>(capacity)?;
    control
        .checked_add(backing)
        .ok_or_else(|| AllocationSizeError::new("Arc<Vec>"))
}

fn arc_allocation_bytes(
    payload_bytes: usize,
    payload_alignment: usize,
    allocation: &'static str,
) -> Result<u64, AllocationSizeError> {
    let counters = size_of::<usize>()
        .checked_mul(2)
        .ok_or_else(|| AllocationSizeError::new(allocation))?;
    // Rust does not expose Arc's allocator layout. Two reference-count words plus one maximum
    // alignment unit conservatively cover the language-visible control block and its padding.
    let alignment_slack = payload_alignment.max(align_of::<usize>());
    let bytes = counters
        .checked_add(payload_bytes)
        .and_then(|bytes| bytes.checked_add(alignment_slack))
        .ok_or_else(|| AllocationSizeError::new(allocation))?;
    usize_to_u64(bytes, allocation)
}

fn usize_to_u64(value: usize, allocation: &'static str) -> Result<u64, AllocationSizeError> {
    u64::try_from(value).map_err(|_| AllocationSizeError::new(allocation))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(align(64))]
    struct Aligned([u8; 64]);

    #[test]
    fn contiguous_backings_use_capacity_not_length_metadata() {
        assert_eq!(vec_allocation_bytes::<u32>(7).unwrap(), 28);
        assert_eq!(string_allocation_bytes(11).unwrap(), 11);
        assert_eq!(vec_allocation_bytes::<()>(usize::MAX).unwrap(), 0);
    }

    #[test]
    fn arc_slice_includes_payload_control_words_and_padding() {
        let estimate = arc_slice_allocation_bytes::<u8>(13).unwrap();
        let minimum = 13_u64 + u64::try_from(size_of::<usize>() * 2).unwrap();
        assert!(estimate > minimum);
    }

    #[test]
    fn empty_and_zst_arcs_still_account_for_the_control_allocation() {
        let empty_slice = arc_slice_allocation_bytes::<u8>(0).unwrap();
        let zst_value = arc_value_allocation_bytes::<()>().unwrap();
        let counters = u64::try_from(size_of::<usize>() * 2).unwrap();

        assert!(empty_slice > counters);
        assert!(zst_value > counters);
    }

    #[test]
    fn highly_aligned_arc_values_include_worst_case_layout_padding() {
        let estimate = arc_value_allocation_bytes::<Aligned>().unwrap();
        let payload_and_counters =
            u64::try_from(size_of::<Aligned>() + size_of::<usize>() * 2).unwrap();

        assert_eq!(align_of::<Aligned>(), 64);
        assert!(estimate >= payload_and_counters + 64);
        assert_eq!(size_of::<Aligned>(), 64);
        let Aligned(bytes) = Aligned([0; 64]);
        assert_eq!(bytes.len(), 64);
    }

    #[test]
    fn arc_vec_includes_control_block_and_separate_vec_backing() {
        assert_eq!(
            arc_vec_allocation_bytes::<u16>(9).unwrap(),
            arc_value_allocation_bytes::<Vec<u16>>().unwrap() + 18
        );
    }

    #[test]
    fn arithmetic_overflow_is_typed() {
        if size_of::<usize>() == size_of::<u64>() {
            let error = vec_allocation_bytes::<u16>(usize::MAX).unwrap_err();
            assert_eq!(error.allocation(), "Vec");

            let error = arc_slice_allocation_bytes::<u16>(usize::MAX).unwrap_err();
            assert_eq!(error.allocation(), "Arc slice");

            let error = arc_vec_allocation_bytes::<u16>(usize::MAX).unwrap_err();
            assert_eq!(error.allocation(), "Vec");
        }
    }
}
