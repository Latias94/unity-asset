//! Fallible sinks shared by SerializedFile sizing and artifact encoding.
//!
//! The old writer used `BinaryWriter` as an in-memory staging buffer.  Prepared artifacts need to
//! write directly into their budgeted seekable chunks instead.  `EndianSink` keeps the wire-level
//! primitive operations in one place while `CountingSink` provides a no-allocation sizing pass.

use std::io::{self, Seek, Write};

use crate::{BinaryWriter, Endian, Result, UnityAssetError};

/// Minimal fallible backend required by the SerializedFile wire encoder.
pub(crate) trait SinkBackend {
    fn position(&mut self) -> Result<u64>;
    fn write_all(&mut self, bytes: &[u8]) -> Result<()>;
}

impl SinkBackend for BinaryWriter {
    fn position(&mut self) -> Result<u64> {
        u64::try_from(BinaryWriter::position(self))
            .map_err(|_| UnityAssetError::format("writer position does not fit u64"))
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.write(bytes);
        self.ensure_valid()
    }
}

impl<T> SinkBackend for &mut T
where
    T: SinkBackend + ?Sized,
{
    fn position(&mut self) -> Result<u64> {
        SinkBackend::position(&mut **self)
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        SinkBackend::write_all(&mut **self, bytes)
    }
}

/// Adapter for budgeted artifact writers and other standard seekable sinks.
pub(crate) struct IoSink<W>(W);

impl<W> IoSink<W> {
    pub(crate) const fn new(writer: W) -> Self {
        Self(writer)
    }
}

impl<W> SinkBackend for IoSink<W>
where
    W: Write + Seek,
{
    fn position(&mut self) -> Result<u64> {
        self.0.stream_position().map_err(io_error)
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        Write::write_all(&mut self.0, bytes).map_err(io_error)
    }
}

/// Primitive writer with an explicit wire byte order.
pub(crate) struct EndianSink<B> {
    backend: B,
    endian: Endian,
}

impl<B> EndianSink<B>
where
    B: SinkBackend,
{
    pub(crate) const fn new(backend: B, endian: Endian) -> Self {
        Self { backend, endian }
    }

    pub(crate) fn into_inner(self) -> B {
        self.backend
    }

    pub(crate) fn position(&mut self) -> Result<u64> {
        self.backend.position()
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.backend.write_all(bytes)
    }

    pub(crate) fn write_u8(&mut self, value: u8) -> Result<()> {
        self.write(&[value])
    }

    pub(crate) fn write_bool(&mut self, value: bool) -> Result<()> {
        self.write_u8(u8::from(value))
    }

    pub(crate) fn write_u16(&mut self, value: u16) -> Result<()> {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.write(&bytes)
    }

    pub(crate) fn write_i16(&mut self, value: i16) -> Result<()> {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.write(&bytes)
    }

    pub(crate) fn write_u32(&mut self, value: u32) -> Result<()> {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.write(&bytes)
    }

    pub(crate) fn write_i32(&mut self, value: i32) -> Result<()> {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.write(&bytes)
    }

    pub(crate) fn write_u64(&mut self, value: u64) -> Result<()> {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.write(&bytes)
    }

    pub(crate) fn write_i64(&mut self, value: i64) -> Result<()> {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.write(&bytes)
    }

    pub(crate) fn write_string_to_null(&mut self, value: &str) -> Result<()> {
        self.write(value.as_bytes())?;
        self.write_u8(0)
    }

    pub(crate) fn align_stream(&mut self, alignment: u64) -> Result<()> {
        if alignment == 0 || !alignment.is_power_of_two() {
            return Err(UnityAssetError::format(format!(
                "stream alignment must be a nonzero power of two: {alignment}"
            )));
        }
        let position = self.position()?;
        let padding = (alignment - (position % alignment)) % alignment;
        let mut remaining = padding;
        let zeros = [0_u8; 64];
        while remaining != 0 {
            let count = remaining.min(zeros.len() as u64) as usize;
            self.write(&zeros[..count])?;
            remaining -= count as u64;
        }
        Ok(())
    }
}

/// A seekable no-allocation backend used for the metadata/data sizing pass.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CountingSink {
    position: u64,
    length: u64,
}

impl CountingSink {
    pub(crate) const fn length(self) -> u64 {
        self.length
    }
}

impl SinkBackend for CountingSink {
    fn position(&mut self) -> Result<u64> {
        Ok(self.position)
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        let amount = u64::try_from(bytes.len())
            .map_err(|_| UnityAssetError::format("counted write length does not fit u64"))?;
        self.position = self
            .position
            .checked_add(amount)
            .ok_or_else(|| UnityAssetError::format("counted writer position overflow"))?;
        self.length = self.length.max(self.position);
        Ok(())
    }
}

fn io_error(error: io::Error) -> UnityAssetError {
    UnityAssetError::with_source("SerializedFile sink I/O error", error)
}
