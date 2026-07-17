use std::ops::Range;

use super::segments::SegmentedBytes;
use super::validate_range;
use crate::error::{BinaryError, Result};

/// The binary crate owns this seam so downstream crates only exchange byte values.
pub(crate) trait ByteSource {
    fn len(&self) -> u64;
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<()>;
    fn contiguous(&self, range: Range<u64>) -> Option<&[u8]>;
}

pub(crate) struct BorrowedBytes<'a> {
    bytes: &'a [u8],
}

impl<'a> BorrowedBytes<'a> {
    pub(crate) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
}

impl ByteSource for BorrowedBytes<'_> {
    fn len(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        let start = usize::try_from(offset)
            .map_err(|_| BinaryError::invalid_data("slice read offset does not fit usize"))?;
        let end = start
            .checked_add(output.len())
            .ok_or_else(|| BinaryError::invalid_data("slice read range overflows usize"))?;
        let bytes = self
            .bytes
            .get(start..end)
            .ok_or_else(|| BinaryError::not_enough_data(end, self.bytes.len()))?;
        output.copy_from_slice(bytes);
        Ok(())
    }

    fn contiguous(&self, range: Range<u64>) -> Option<&[u8]> {
        let start = usize::try_from(range.start).ok()?;
        let end = usize::try_from(range.end).ok()?;
        self.bytes.get(start..end)
    }
}

impl ByteSource for SegmentedBytes {
    fn len(&self) -> u64 {
        self.len()
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        let output_len = u64::try_from(output.len())
            .map_err(|_| BinaryError::invalid_data("read length does not fit in u64"))?;
        let end = offset
            .checked_add(output_len)
            .ok_or_else(|| BinaryError::invalid_data("random-access read range overflows u64"))?;
        validate_range("random-access read", &(offset..end), self.len())?;
        if output.is_empty() {
            return Ok(());
        }

        if let Some(bytes) = self.contiguous_range(offset..end) {
            output.copy_from_slice(bytes);
            return Ok(());
        }

        let mut segment_index = self.segment_index(offset).ok_or_else(|| {
            BinaryError::invalid_data(format!(
                "random-access read starts outside the image at {offset}"
            ))
        })?;
        let mut logical_offset = offset;
        let mut written = 0_usize;

        while written < output.len() {
            let segment = self.segments().get(segment_index).ok_or_else(|| {
                BinaryError::invalid_data("segmented byte image ended during an exact read")
            })?;
            if logical_offset < segment.logical_start() || logical_offset >= segment.logical_end() {
                return Err(BinaryError::invalid_data(format!(
                    "segmented byte image has a gap at logical offset {logical_offset}"
                )));
            }

            let available = segment.logical_end() - logical_offset;
            let remaining = u64::try_from(output.len() - written)
                .map_err(|_| BinaryError::invalid_data("read length does not fit in u64"))?;
            let take = available.min(remaining);
            let take_usize = usize::try_from(take).map_err(|_| {
                BinaryError::invalid_data("segment read length does not fit in usize")
            })?;
            let chunk_end = logical_offset
                .checked_add(take)
                .ok_or_else(|| BinaryError::invalid_data("segment read range overflows u64"))?;
            let chunk = segment
                .contiguous_range(logical_offset..chunk_end)
                .ok_or_else(|| BinaryError::invalid_data("invalid segment backing range"))?;
            output[written..written + take_usize].copy_from_slice(chunk);

            written += take_usize;
            logical_offset = chunk_end;
            segment_index += 1;
        }

        Ok(())
    }

    fn contiguous(&self, range: Range<u64>) -> Option<&[u8]> {
        self.contiguous_range(range)
    }
}
