use std::ops::Range;

use unity_asset_core::AssetLoadBudget;

use super::source::ByteSource;
use super::{allocation_error, validate_range};
use crate::byte_order::ByteOrder;
use crate::error::{BinaryError, Result};

const CSTRING_SCAN_CHUNK: usize = 8 * 1024;

/// A bounded parser cursor whose positions remain absolute within its source.
///
/// This stays crate-private so format modules expose structured parse results,
/// not storage mechanics.
pub(crate) struct ByteCursor<'source, 'budget> {
    source: &'source dyn ByteSource,
    contiguous: Option<&'source [u8]>,
    range: Range<u64>,
    position: u64,
    byte_order: ByteOrder,
    budget: &'budget mut AssetLoadBudget,
}

impl<'source, 'budget> ByteCursor<'source, 'budget> {
    pub(crate) fn new(
        source: &'source dyn ByteSource,
        byte_order: ByteOrder,
        budget: &'budget mut AssetLoadBudget,
    ) -> Result<Self> {
        Self::with_range(source, 0..source.len(), byte_order, budget)
    }

    pub(crate) fn with_range(
        source: &'source dyn ByteSource,
        range: Range<u64>,
        byte_order: ByteOrder,
        budget: &'budget mut AssetLoadBudget,
    ) -> Result<Self> {
        validate_range("cursor range", &range, source.len())?;
        let position = range.start;
        let contiguous = source.contiguous(range.clone());
        Ok(Self {
            source,
            contiguous,
            range,
            position,
            byte_order,
            budget,
        })
    }

    pub(crate) fn position(&self) -> u64 {
        self.position
    }

    pub(crate) fn remaining(&self) -> u64 {
        self.range.end - self.position
    }

    /// Moves within the bounded view without reading or charging source bytes.
    pub(crate) fn set_position(&mut self, position: u64) -> Result<()> {
        if position < self.range.start || position > self.range.end {
            return Err(BinaryError::invalid_data(format!(
                "cursor position {position} is outside bounded range {}..{}",
                self.range.start, self.range.end
            )));
        }
        self.position = position;
        Ok(())
    }

    /// Skips to an absolute alignment boundary without reading or validating padding.
    pub(crate) fn align_to(&mut self, alignment: u64) -> Result<()> {
        if alignment == 0 {
            return Err(BinaryError::invalid_data(
                "cursor alignment must be nonzero",
            ));
        }
        let remainder = self.position % alignment;
        if remainder == 0 {
            return Ok(());
        }
        let padding = alignment - remainder;
        let aligned = self
            .position
            .checked_add(padding)
            .ok_or_else(|| BinaryError::invalid_data("cursor alignment overflows u64"))?;
        if aligned > self.range.end {
            return Err(BinaryError::invalid_data(format!(
                "cursor alignment to {alignment} would leave bounded range {}..{}",
                self.range.start, self.range.end
            )));
        }
        self.position = aligned;
        Ok(())
    }

    pub(crate) fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_array::<1>()?[0])
    }

    pub(crate) fn read_u16(&mut self) -> Result<u16> {
        let bytes = self.read_array::<2>()?;
        Ok(match self.byte_order {
            ByteOrder::Big => u16::from_be_bytes(bytes),
            ByteOrder::Little => u16::from_le_bytes(bytes),
        })
    }

    pub(crate) fn read_i16(&mut self) -> Result<i16> {
        let bytes = self.read_array::<2>()?;
        Ok(match self.byte_order {
            ByteOrder::Big => i16::from_be_bytes(bytes),
            ByteOrder::Little => i16::from_le_bytes(bytes),
        })
    }

    pub(crate) fn read_u32(&mut self) -> Result<u32> {
        let bytes = self.read_array::<4>()?;
        Ok(match self.byte_order {
            ByteOrder::Big => u32::from_be_bytes(bytes),
            ByteOrder::Little => u32::from_le_bytes(bytes),
        })
    }

    pub(crate) fn read_i32(&mut self) -> Result<i32> {
        let bytes = self.read_array::<4>()?;
        Ok(match self.byte_order {
            ByteOrder::Big => i32::from_be_bytes(bytes),
            ByteOrder::Little => i32::from_le_bytes(bytes),
        })
    }

    pub(crate) fn read_u64(&mut self) -> Result<u64> {
        let bytes = self.read_array::<8>()?;
        Ok(match self.byte_order {
            ByteOrder::Big => u64::from_be_bytes(bytes),
            ByteOrder::Little => u64::from_le_bytes(bytes),
        })
    }

    pub(crate) fn read_i64(&mut self) -> Result<i64> {
        let bytes = self.read_array::<8>()?;
        Ok(match self.byte_order {
            ByteOrder::Big => i64::from_be_bytes(bytes),
            ByteOrder::Little => i64::from_le_bytes(bytes),
        })
    }

    pub(crate) fn read_bytes(&mut self, count: u64) -> Result<Vec<u8>> {
        let end = self.checked_read_end(count)?;
        let count_usize = usize::try_from(count)
            .map_err(|_| BinaryError::memory_error("read length does not fit in usize"))?;
        self.charge_bytes(count)?;

        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(count_usize)
            .map_err(|error| allocation_error(count_usize, error))?;
        bytes.resize(count_usize, 0);
        self.read_exact_at(self.position, &mut bytes)?;
        self.position = end;
        Ok(bytes)
    }

    /// Scans within both the format limit and remaining byte budget.
    ///
    /// A failed scan leaves the cursor position unchanged, while bytes already inspected remain
    /// charged to prevent retries from rescanning untrusted input for free.
    pub(crate) fn read_cstring(&mut self, max_len: u64) -> Result<String> {
        let start = self.position;
        let format_scan_limit = max_len
            .checked_add(1)
            .ok_or_else(|| BinaryError::invalid_data("C string length limit overflows u64"))?
            .min(self.remaining());
        let remaining_budget = self.budget.remaining_bytes();
        let maximum_scan = format_scan_limit.min(remaining_budget);
        let scan_end = start
            .checked_add(maximum_scan)
            .ok_or_else(|| BinaryError::invalid_data("C string scan range overflows u64"))?;
        let nul = self.scan_cstring(start..scan_end)?;

        let Some((nul_position, bytes)) = nul else {
            if maximum_scan < format_scan_limit {
                self.charge_bytes(1)?;
                return Err(BinaryError::invalid_data(
                    "C string scan stopped before its format limit",
                ));
            }
            if self.remaining() > max_len {
                return Err(BinaryError::invalid_data(format!(
                    "C string exceeds maximum length {max_len}"
                )));
            }
            return Err(BinaryError::invalid_data(format!(
                "unterminated C string in bounded range {}..{}",
                self.range.start, self.range.end
            )));
        };

        self.position = nul_position
            .checked_add(1)
            .ok_or_else(|| BinaryError::invalid_data("C string end offset overflows u64"))?;
        Ok(String::from_utf8(bytes)?)
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let count = u64::try_from(N)
            .map_err(|_| BinaryError::invalid_data("primitive width does not fit in u64"))?;
        let end = self.checked_read_end(count)?;
        self.charge_bytes(count)?;
        let mut bytes = [0_u8; N];
        self.read_exact_at(self.position, &mut bytes)?;
        self.position = end;
        Ok(bytes)
    }

    fn checked_read_end(&self, count: u64) -> Result<u64> {
        let end = self
            .position
            .checked_add(count)
            .ok_or_else(|| BinaryError::invalid_data("cursor read range overflows u64"))?;
        if end > self.range.end {
            return Err(BinaryError::invalid_data(format!(
                "cursor read {}..{end} leaves bounded range {}..{}",
                self.position, self.range.start, self.range.end
            )));
        }
        Ok(end)
    }

    fn scan_cstring(&mut self, range: Range<u64>) -> Result<Option<(u64, Vec<u8>)>> {
        if let Some(contiguous) = self.contiguous {
            let relative_start = range
                .start
                .checked_sub(self.range.start)
                .ok_or_else(|| BinaryError::invalid_data("C string scan precedes cursor range"))?;
            let relative_end = range
                .end
                .checked_sub(self.range.start)
                .ok_or_else(|| BinaryError::invalid_data("C string scan precedes cursor range"))?;
            let relative_start = usize::try_from(relative_start).map_err(|_| {
                BinaryError::invalid_data("C string scan start does not fit in usize")
            })?;
            let relative_end = usize::try_from(relative_end).map_err(|_| {
                BinaryError::invalid_data("C string scan end does not fit in usize")
            })?;
            let bytes = contiguous
                .get(relative_start..relative_end)
                .ok_or_else(|| {
                    BinaryError::invalid_data(
                        "contiguous source returned an incomplete cursor range",
                    )
                })?;

            let Some(relative) = bytes.iter().position(|byte| *byte == 0) else {
                self.charge_bytes(u64::try_from(bytes.len()).map_err(|_| {
                    BinaryError::invalid_data("C string scan length does not fit in u64")
                })?)?;
                return Ok(None);
            };

            let inspected = relative.checked_add(1).ok_or_else(|| {
                BinaryError::invalid_data("C string inspected length overflows usize")
            })?;
            self.charge_bytes(u64::try_from(inspected).map_err(|_| {
                BinaryError::invalid_data("C string inspected length does not fit in u64")
            })?)?;
            let mut owned = Vec::new();
            owned
                .try_reserve(relative)
                .map_err(|error| allocation_error(relative, error))?;
            owned.extend_from_slice(&bytes[..relative]);
            let relative = u64::try_from(relative).map_err(|_| {
                BinaryError::invalid_data("C string terminator offset does not fit in u64")
            })?;
            let nul_position = range
                .start
                .checked_add(relative)
                .ok_or_else(|| BinaryError::invalid_data("C string offset overflows u64"))?;
            return Ok(Some((nul_position, owned)));
        }

        let mut offset = range.start;
        let mut scratch = [0_u8; CSTRING_SCAN_CHUNK];
        let mut bytes = Vec::new();
        while offset < range.end {
            let remaining = range.end - offset;
            let chunk_len = remaining.min(CSTRING_SCAN_CHUNK as u64);
            let chunk_len_usize = usize::try_from(chunk_len).map_err(|_| {
                BinaryError::invalid_data("C string scan length does not fit in usize")
            })?;
            let chunk = &mut scratch[..chunk_len_usize];
            self.read_exact_at(offset, chunk)?;
            if let Some(relative) = chunk.iter().position(|byte| *byte == 0) {
                let inspected = relative.checked_add(1).ok_or_else(|| {
                    BinaryError::invalid_data("C string inspected length overflows usize")
                })?;
                self.charge_bytes(u64::try_from(inspected).map_err(|_| {
                    BinaryError::invalid_data("C string inspected length does not fit in u64")
                })?)?;
                bytes
                    .try_reserve(relative)
                    .map_err(|error| allocation_error(relative, error))?;
                bytes.extend_from_slice(&chunk[..relative]);
                let relative = u64::try_from(relative).map_err(|_| {
                    BinaryError::invalid_data("C string terminator offset does not fit in u64")
                })?;
                let nul_position = offset
                    .checked_add(relative)
                    .ok_or_else(|| BinaryError::invalid_data("C string offset overflows u64"))?;
                return Ok(Some((nul_position, bytes)));
            }
            self.charge_bytes(chunk_len)?;
            bytes
                .try_reserve(chunk_len_usize)
                .map_err(|error| allocation_error(chunk_len_usize, error))?;
            bytes.extend_from_slice(chunk);
            offset = offset
                .checked_add(chunk_len)
                .ok_or_else(|| BinaryError::invalid_data("C string scan offset overflows u64"))?;
        }
        Ok(None)
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        let Some(contiguous) = self.contiguous else {
            return self.source.read_exact_at(offset, output);
        };
        let relative_start = offset
            .checked_sub(self.range.start)
            .ok_or_else(|| BinaryError::invalid_data("cursor read precedes contiguous range"))?;
        let start = usize::try_from(relative_start).map_err(|_| {
            BinaryError::invalid_data("contiguous cursor offset does not fit in usize")
        })?;
        let end = start
            .checked_add(output.len())
            .ok_or_else(|| BinaryError::invalid_data("contiguous cursor range overflows usize"))?;
        let bytes = contiguous.get(start..end).ok_or_else(|| {
            BinaryError::invalid_data("contiguous source returned an incomplete cursor range")
        })?;
        output.copy_from_slice(bytes);
        Ok(())
    }

    fn charge_bytes(&mut self, amount: u64) -> Result<()> {
        self.budget.consume_bytes(amount).map_err(Into::into)
    }

    pub(crate) fn consume_bytes(&mut self, amount: u64) -> Result<()> {
        self.charge_bytes(amount)
    }

    pub(crate) fn consume_entries(&mut self, amount: u64) -> Result<()> {
        self.budget.consume_entries(amount).map_err(Into::into)
    }

    pub(crate) fn consume_members(&mut self, amount: u64) -> Result<()> {
        self.budget.consume_members(amount).map_err(Into::into)
    }

    pub(crate) fn observe_depth(&mut self, depth: u32) -> Result<()> {
        self.budget.observe_depth(depth).map_err(Into::into)
    }
}
