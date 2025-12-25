use crate::error::{BinaryError, Result};
use std::ops::Range;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct DataView {
    data: Arc<[u8]>,
    start: usize,
    len: usize,
}

impl DataView {
    pub fn from_arc(data: Arc<[u8]>) -> Self {
        let len = data.len();
        Self { data, start: 0, len }
    }

    pub fn from_range(data: Arc<[u8]>, range: Range<usize>) -> Result<Self> {
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
        &self.data[self.start..self.start + self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn backing_arc(&self) -> Arc<[u8]> {
        self.data.clone()
    }

    pub fn base_offset(&self) -> usize {
        self.start
    }

    pub fn identity_key(&self) -> (usize, usize, usize) {
        (self.data.as_ref().as_ptr() as usize, self.start, self.len)
    }
}
