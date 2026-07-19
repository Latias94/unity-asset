//! Immutable segmented byte values and the crate-private random-access parse seam.

use std::ops::Range;

use crate::error::{BinaryError, Result};

mod cursor;
mod reader;
mod segments;
mod source;

pub(crate) use cursor::ByteCursor;
pub(crate) use reader::{ByteSourceReader, FallibleBufReader};
pub use segments::{ByteSegment, SegmentedBytes};
pub(crate) use source::{BorrowedBytes, ByteSource};

fn validate_range(context: &str, range: &Range<u64>, limit: u64) -> Result<()> {
    if range.start > range.end || range.end > limit {
        return Err(BinaryError::invalid_data(format!(
            "{context} {}..{} is outside 0..{limit}",
            range.start, range.end
        )));
    }
    Ok(())
}

fn allocation_error(requested: usize, error: std::collections::TryReserveError) -> BinaryError {
    BinaryError::memory_error(format!(
        "failed to reserve {requested} bytes for segmented random access: {error}"
    ))
}

#[cfg(test)]
mod tests;
