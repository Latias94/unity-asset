use std::io::{self, BufRead, Read};
use std::ops::Range;

use super::source::ByteSource;

/// Sequential adapter for codecs that accept `Read` but must retain segmented input.
pub(crate) struct ByteSourceReader<'source> {
    source: &'source dyn ByteSource,
    range: Range<u64>,
    position: u64,
    bytes_read: u64,
}

impl<'source> ByteSourceReader<'source> {
    pub(crate) fn new(source: &'source dyn ByteSource) -> Self {
        Self {
            source,
            range: 0..source.len(),
            position: 0,
            bytes_read: 0,
        }
    }

    pub(crate) fn with_range(
        source: &'source dyn ByteSource,
        range: Range<u64>,
    ) -> crate::error::Result<Self> {
        super::validate_range("sequential source range", &range, source.len())?;
        let position = range.start;
        Ok(Self {
            source,
            range,
            position,
            bytes_read: 0,
        })
    }

    pub(crate) fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
}

impl Read for ByteSourceReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let remaining = self.range.end - self.position;
        if remaining == 0 || output.is_empty() {
            return Ok(0);
        }
        let output_len = u64::try_from(output.len()).unwrap_or(u64::MAX);
        let read_len = remaining.min(output_len);
        let read_len = usize::try_from(read_len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "sequential source read length does not fit usize",
            )
        })?;
        self.source
            .read_exact_at(self.position, &mut output[..read_len])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.position = self
            .position
            .checked_add(u64::try_from(read_len).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "read length does not fit u64")
            })?)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "source position overflow")
            })?;
        self.bytes_read = self
            .bytes_read
            .checked_add(u64::try_from(read_len).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "read length does not fit u64")
            })?)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "source read counter overflow")
            })?;
        Ok(read_len)
    }
}

/// A `BufRead` adapter whose fixed backing allocation is explicitly fallible.
pub(crate) struct FallibleBufReader<R> {
    inner: R,
    buffer: Vec<u8>,
    position: usize,
    filled: usize,
}

impl<R> FallibleBufReader<R> {
    pub(crate) fn try_with_capacity(capacity: usize, inner: R) -> crate::error::Result<Self> {
        let mut buffer = Vec::new();
        buffer.try_reserve_exact(capacity).map_err(|error| {
            crate::error::BinaryError::memory_error(format!(
                "Failed to reserve {capacity} bytes for a codec input buffer: {error}"
            ))
        })?;
        buffer.resize(capacity, 0);
        Ok(Self {
            inner,
            buffer,
            position: 0,
            filled: 0,
        })
    }

    pub(crate) const fn get_ref(&self) -> &R {
        &self.inner
    }

    pub(crate) fn buffer(&self) -> &[u8] {
        &self.buffer[self.position..self.filled]
    }
}

impl<R: Read> Read for FallibleBufReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.position == self.filled && output.len() >= self.buffer.len() {
            return self.inner.read(output);
        }

        let available = self.fill_buf()?;
        let read_len = available.len().min(output.len());
        output[..read_len].copy_from_slice(&available[..read_len]);
        self.consume(read_len);
        Ok(read_len)
    }
}

impl<R: Read> BufRead for FallibleBufReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.position == self.filled {
            self.filled = self.inner.read(&mut self.buffer)?;
            self.position = 0;
        }
        Ok(&self.buffer[self.position..self.filled])
    }

    fn consume(&mut self, amount: usize) {
        self.position = self.position.saturating_add(amount).min(self.filled);
    }
}
