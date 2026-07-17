use crate::error::{BinaryError, Result};
use crate::random_access::ByteSource;
use crate::shared_bytes::SharedBytes;
use std::ops::Range;

#[derive(Debug, Clone)]
pub struct DataView {
    data: SharedBytes,
    start: usize,
    len: usize,
}

impl DataView {
    pub fn from_shared(data: SharedBytes) -> Self {
        let len = data.len();
        Self {
            data,
            start: 0,
            len,
        }
    }

    pub fn from_shared_range(data: SharedBytes, range: Range<usize>) -> Result<Self> {
        if range.start > range.end {
            return Err(BinaryError::invalid_data(format!(
                "Invalid DataView range: {}..{}",
                range.start, range.end
            )));
        }
        let total = data.len();
        if range.end > total {
            return Err(BinaryError::not_enough_data(range.end, total));
        }
        Ok(Self {
            data,
            start: range.start,
            len: range.end - range.start,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data.as_bytes()[self.start..self.start + self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn backing_shared(&self) -> SharedBytes {
        self.data.clone()
    }

    pub fn base_offset(&self) -> usize {
        self.start
    }

    pub fn absolute_range(&self) -> Range<usize> {
        self.start..self.start + self.len
    }

    pub fn identity_key(&self) -> (usize, usize, usize) {
        (self.data.ptr_usize(), self.start, self.len)
    }
}

impl ByteSource for DataView {
    fn len(&self) -> u64 {
        self.len as u64
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        let start = usize::try_from(offset)
            .map_err(|_| BinaryError::invalid_data("DataView offset does not fit in usize"))?;
        let end = start
            .checked_add(output.len())
            .ok_or_else(|| BinaryError::invalid_data("DataView read range overflows usize"))?;
        let bytes = self
            .as_bytes()
            .get(start..end)
            .ok_or_else(|| BinaryError::not_enough_data(end, self.len))?;
        output.copy_from_slice(bytes);
        Ok(())
    }

    fn contiguous(&self, range: Range<u64>) -> Option<&[u8]> {
        let start = usize::try_from(range.start).ok()?;
        let end = usize::try_from(range.end).ok()?;
        self.as_bytes().get(start..end)
    }
}
