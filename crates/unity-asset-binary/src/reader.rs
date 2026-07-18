//! Binary data reader for Unity files

pub use crate::byte_order::ByteOrder;
use crate::error::{BinaryError, Result};
use crate::random_access::ByteCursor;
use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use std::io::{Cursor, Read, Seek, SeekFrom};

/// Primitive binary reading without resource-allocation semantics.
pub(crate) trait BinaryRead {
    fn position(&self) -> u64;
    fn remaining(&self) -> u64;
    fn set_position(&mut self, position: u64) -> Result<()>;
    fn align_to(&mut self, alignment: u64) -> Result<()>;
    fn read_u8(&mut self) -> Result<u8>;
    fn read_u16(&mut self) -> Result<u16>;
    fn read_i16(&mut self) -> Result<i16>;
    fn read_u32(&mut self) -> Result<u32>;
    fn read_i32(&mut self) -> Result<i32>;
    fn read_u64(&mut self) -> Result<u64>;
    fn read_i64(&mut self) -> Result<i64>;
    fn read_bytes(&mut self, count: usize) -> Result<Vec<u8>>;
    fn read_cstring_limited(&mut self, max_len: usize) -> Result<String>;
    fn read_bool(&mut self) -> Result<bool> {
        Ok(self.read_u8()? != 0)
    }

    fn align(&mut self) -> Result<()> {
        self.align_to(4)
    }
}

/// Parser input that must account for allocations, hierarchy, and collection growth.
///
/// Allocation-bearing parsers accept this trait, so a plain [`BinaryReader`] cannot silently
/// bypass a caller-owned `AssetLoadBudget`.
pub(crate) trait BinaryInput: BinaryRead {
    fn check_bytes(&self, amount: u64) -> Result<()>;
    fn check_entries(&self, amount: u64) -> Result<()>;
    fn check_members(&self, amount: u64) -> Result<()>;
    fn check_depth(&self, depth: u32) -> Result<()>;
    fn consume_bytes(&mut self, amount: u64) -> Result<()>;
    fn consume_entries(&mut self, amount: u64) -> Result<()>;
    fn consume_members(&mut self, amount: u64) -> Result<()>;
    fn observe_depth(&mut self, depth: u32) -> Result<()>;
}

/// Binary reader for Unity file formats
pub struct BinaryReader<'a> {
    cursor: Cursor<&'a [u8]>,
    byte_order: ByteOrder,
}

impl<'a> BinaryReader<'a> {
    /// Default maximum length for length-prefixed strings.
    ///
    /// Unity files can contain large text blobs (e.g. TextAsset), but unbounded allocations are a
    /// DoS risk when parsing hostile input.
    pub const DEFAULT_MAX_STRING_LEN: usize = 16 * 1024 * 1024; // 16 MiB

    /// Create a new binary reader from byte slice
    pub fn new(data: &'a [u8], byte_order: ByteOrder) -> Self {
        Self {
            cursor: Cursor::new(data),
            byte_order,
        }
    }

    /// Get current position in the stream
    pub fn position(&self) -> u64 {
        self.cursor.position()
    }

    /// Set position in the stream
    pub fn set_position(&mut self, pos: u64) -> Result<()> {
        let len = u64::try_from(self.len())
            .map_err(|_| BinaryError::invalid_data("reader length does not fit in u64"))?;
        if pos > len {
            return Err(BinaryError::invalid_data(format!(
                "reader position {pos} is outside 0..{len}"
            )));
        }
        self.cursor.set_position(pos);
        Ok(())
    }

    /// Seek to a position relative to the current position
    pub fn seek(&mut self, offset: i64) -> Result<u64> {
        Ok(self.cursor.seek(SeekFrom::Current(offset))?)
    }

    /// Get the total length of the data
    pub fn len(&self) -> usize {
        self.cursor.get_ref().len()
    }

    /// Check if the reader is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get remaining bytes from current position
    pub fn remaining(&self) -> usize {
        self.len().saturating_sub(self.position() as usize)
    }

    /// Check if we have at least `count` bytes remaining
    pub fn has_bytes(&self, count: usize) -> bool {
        self.remaining() >= count
    }

    /// Align to the next 4-byte boundary
    pub fn align(&mut self) -> Result<()> {
        self.align_to(4)
    }

    /// Align to the specified byte boundary
    pub fn align_to(&mut self, alignment: u64) -> Result<()> {
        if alignment == 0 {
            return Err(BinaryError::invalid_data(
                "reader alignment must be nonzero",
            ));
        }
        let pos = self.position();
        let remainder = pos % alignment;
        let aligned = if remainder == 0 {
            pos
        } else {
            pos.checked_add(alignment - remainder)
                .ok_or_else(|| BinaryError::invalid_data("reader alignment overflows u64"))?
        };
        if aligned != pos {
            self.set_position(aligned)?;
        }
        Ok(())
    }

    /// Read a single byte
    pub fn read_u8(&mut self) -> Result<u8> {
        if !self.has_bytes(1) {
            return Err(BinaryError::not_enough_data(1, self.remaining()));
        }
        Ok(self.cursor.read_u8()?)
    }

    /// Read a boolean (as u8, 0 = false, non-zero = true)
    pub fn read_bool(&mut self) -> Result<bool> {
        Ok(self.read_u8()? != 0)
    }

    /// Read a signed 8-bit integer
    pub fn read_i8(&mut self) -> Result<i8> {
        Ok(self.read_u8()? as i8)
    }

    /// Read an unsigned 16-bit integer
    pub fn read_u16(&mut self) -> Result<u16> {
        if !self.has_bytes(2) {
            return Err(BinaryError::not_enough_data(2, self.remaining()));
        }
        match self.byte_order {
            ByteOrder::Big => Ok(self.cursor.read_u16::<BigEndian>()?),
            ByteOrder::Little => Ok(self.cursor.read_u16::<LittleEndian>()?),
        }
    }

    /// Read a signed 16-bit integer
    pub fn read_i16(&mut self) -> Result<i16> {
        if !self.has_bytes(2) {
            return Err(BinaryError::not_enough_data(2, self.remaining()));
        }
        match self.byte_order {
            ByteOrder::Big => Ok(self.cursor.read_i16::<BigEndian>()?),
            ByteOrder::Little => Ok(self.cursor.read_i16::<LittleEndian>()?),
        }
    }

    /// Read an unsigned 32-bit integer
    pub fn read_u32(&mut self) -> Result<u32> {
        if !self.has_bytes(4) {
            return Err(BinaryError::not_enough_data(4, self.remaining()));
        }
        match self.byte_order {
            ByteOrder::Big => Ok(self.cursor.read_u32::<BigEndian>()?),
            ByteOrder::Little => Ok(self.cursor.read_u32::<LittleEndian>()?),
        }
    }

    /// Read a signed 32-bit integer
    pub fn read_i32(&mut self) -> Result<i32> {
        if !self.has_bytes(4) {
            return Err(BinaryError::not_enough_data(4, self.remaining()));
        }
        match self.byte_order {
            ByteOrder::Big => Ok(self.cursor.read_i32::<BigEndian>()?),
            ByteOrder::Little => Ok(self.cursor.read_i32::<LittleEndian>()?),
        }
    }

    /// Read an unsigned 64-bit integer
    pub fn read_u64(&mut self) -> Result<u64> {
        if !self.has_bytes(8) {
            return Err(BinaryError::not_enough_data(8, self.remaining()));
        }
        match self.byte_order {
            ByteOrder::Big => Ok(self.cursor.read_u64::<BigEndian>()?),
            ByteOrder::Little => Ok(self.cursor.read_u64::<LittleEndian>()?),
        }
    }

    /// Read a signed 64-bit integer
    pub fn read_i64(&mut self) -> Result<i64> {
        if !self.has_bytes(8) {
            return Err(BinaryError::not_enough_data(8, self.remaining()));
        }
        match self.byte_order {
            ByteOrder::Big => Ok(self.cursor.read_i64::<BigEndian>()?),
            ByteOrder::Little => Ok(self.cursor.read_i64::<LittleEndian>()?),
        }
    }

    /// Read a 32-bit floating point number
    pub fn read_f32(&mut self) -> Result<f32> {
        if !self.has_bytes(4) {
            return Err(BinaryError::not_enough_data(4, self.remaining()));
        }
        match self.byte_order {
            ByteOrder::Big => Ok(self.cursor.read_f32::<BigEndian>()?),
            ByteOrder::Little => Ok(self.cursor.read_f32::<LittleEndian>()?),
        }
    }

    /// Read a 64-bit floating point number
    pub fn read_f64(&mut self) -> Result<f64> {
        if !self.has_bytes(8) {
            return Err(BinaryError::not_enough_data(8, self.remaining()));
        }
        match self.byte_order {
            ByteOrder::Big => Ok(self.cursor.read_f64::<BigEndian>()?),
            ByteOrder::Little => Ok(self.cursor.read_f64::<LittleEndian>()?),
        }
    }

    /// Read a fixed number of bytes
    pub fn read_bytes(&mut self, count: usize) -> Result<Vec<u8>> {
        if !self.has_bytes(count) {
            return Err(BinaryError::not_enough_data(count, self.remaining()));
        }
        let mut buffer = vec![0u8; count];
        self.cursor.read_exact(&mut buffer)?;
        Ok(buffer)
    }

    /// Skip a fixed number of bytes without allocating.
    pub fn skip_bytes(&mut self, count: usize) -> Result<()> {
        if !self.has_bytes(count) {
            return Err(BinaryError::not_enough_data(count, self.remaining()));
        }
        self.seek(count as i64)?;
        Ok(())
    }

    /// Read all remaining bytes
    pub fn read_remaining(&mut self) -> &[u8] {
        let pos = self.cursor.position() as usize;
        let data = self.cursor.get_ref();
        &data[pos..]
    }

    /// Read a null-terminated string
    pub fn read_cstring(&mut self) -> Result<String> {
        self.read_cstring_limited(Self::DEFAULT_MAX_STRING_LEN)
    }

    /// Read a null-terminated string without scanning past an explicit format limit.
    pub fn read_cstring_limited(&mut self, max_len: usize) -> Result<String> {
        let remaining = self.remaining_slice();
        let scan_len = remaining.len().min(max_len.saturating_add(1));
        let Some(length) = remaining[..scan_len].iter().position(|byte| *byte == 0) else {
            return if remaining.len() > max_len {
                Err(BinaryError::invalid_data(format!(
                    "C string exceeds maximum length {max_len}"
                )))
            } else {
                Err(BinaryError::invalid_data(
                    "unterminated C string in bounded input",
                ))
            };
        };
        let bytes = self.read_bytes(length)?;
        let terminator = self.read_u8()?;
        debug_assert_eq!(terminator, 0);
        Ok(String::from_utf8(bytes)?)
    }

    /// Read a string with a length prefix (32-bit)
    pub fn read_string(&mut self) -> Result<String> {
        self.read_string_limited(Self::DEFAULT_MAX_STRING_LEN)
    }

    /// Read a string with a length prefix and an explicit maximum size.
    ///
    /// Unity typically encodes these lengths as signed 32-bit integers.
    pub fn read_string_limited(&mut self, max_len: usize) -> Result<String> {
        let length = self.read_i32()?;
        if length < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative string length: {}",
                length
            )));
        }

        let length: usize = length as usize;
        if length > max_len {
            return Err(BinaryError::invalid_data(format!(
                "String length {} exceeds limit {}",
                length, max_len
            )));
        }

        // Hard check against remaining to avoid allocating huge buffers just to fail later.
        let remaining = self.remaining();
        if length > remaining {
            return Err(BinaryError::not_enough_data(length, remaining));
        }

        let bytes = self.read_bytes(length)?;
        Ok(String::from_utf8(bytes)?)
    }

    /// Read a string with a specific length
    pub fn read_string_fixed(&mut self, length: usize) -> Result<String> {
        let bytes = self.read_bytes(length)?;
        // Remove null terminators
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        Ok(String::from_utf8(bytes[..end].to_vec())?)
    }

    /// Read an aligned string (Unity format)
    pub fn read_aligned_string(&mut self) -> Result<String> {
        let string = self.read_string()?;
        // Align to 4-byte boundary
        self.align()?;
        Ok(string)
    }

    /// Get the current byte order
    pub fn byte_order(&self) -> ByteOrder {
        self.byte_order
    }

    /// Set the byte order
    pub fn set_byte_order(&mut self, byte_order: ByteOrder) {
        self.byte_order = byte_order;
    }

    /// Get a slice of the remaining data
    pub fn remaining_slice(&self) -> &[u8] {
        let pos = self.position() as usize;
        &self.cursor.get_ref()[pos..]
    }

    /// Create a new reader for a subset of the data
    pub fn sub_reader(&self, offset: usize, length: usize) -> Result<BinaryReader<'a>> {
        let data = self.cursor.get_ref();
        if offset + length > data.len() {
            return Err(BinaryError::not_enough_data(offset + length, data.len()));
        }
        Ok(BinaryReader::new(
            &data[offset..offset + length],
            self.byte_order,
        ))
    }
}

impl BinaryRead for BinaryReader<'_> {
    fn position(&self) -> u64 {
        BinaryReader::position(self)
    }

    fn remaining(&self) -> u64 {
        self.remaining() as u64
    }

    fn set_position(&mut self, position: u64) -> Result<()> {
        BinaryReader::set_position(self, position)
    }

    fn align_to(&mut self, alignment: u64) -> Result<()> {
        BinaryReader::align_to(self, alignment)
    }

    fn read_u8(&mut self) -> Result<u8> {
        BinaryReader::read_u8(self)
    }

    fn read_u16(&mut self) -> Result<u16> {
        BinaryReader::read_u16(self)
    }

    fn read_i16(&mut self) -> Result<i16> {
        BinaryReader::read_i16(self)
    }

    fn read_u32(&mut self) -> Result<u32> {
        BinaryReader::read_u32(self)
    }

    fn read_i32(&mut self) -> Result<i32> {
        BinaryReader::read_i32(self)
    }

    fn read_u64(&mut self) -> Result<u64> {
        BinaryReader::read_u64(self)
    }

    fn read_i64(&mut self) -> Result<i64> {
        BinaryReader::read_i64(self)
    }

    fn read_bytes(&mut self, count: usize) -> Result<Vec<u8>> {
        BinaryReader::read_bytes(self, count)
    }

    fn read_cstring_limited(&mut self, max_len: usize) -> Result<String> {
        BinaryReader::read_cstring_limited(self, max_len)
    }
}

impl BinaryRead for ByteCursor<'_, '_> {
    fn position(&self) -> u64 {
        ByteCursor::position(self)
    }

    fn remaining(&self) -> u64 {
        ByteCursor::remaining(self)
    }

    fn set_position(&mut self, position: u64) -> Result<()> {
        ByteCursor::set_position(self, position)
    }

    fn align_to(&mut self, alignment: u64) -> Result<()> {
        ByteCursor::align_to(self, alignment)
    }

    fn read_u8(&mut self) -> Result<u8> {
        ByteCursor::read_u8(self)
    }

    fn read_u16(&mut self) -> Result<u16> {
        ByteCursor::read_u16(self)
    }

    fn read_i16(&mut self) -> Result<i16> {
        ByteCursor::read_i16(self)
    }

    fn read_u32(&mut self) -> Result<u32> {
        ByteCursor::read_u32(self)
    }

    fn read_i32(&mut self) -> Result<i32> {
        ByteCursor::read_i32(self)
    }

    fn read_u64(&mut self) -> Result<u64> {
        ByteCursor::read_u64(self)
    }

    fn read_i64(&mut self) -> Result<i64> {
        ByteCursor::read_i64(self)
    }

    fn read_bytes(&mut self, count: usize) -> Result<Vec<u8>> {
        let count = u64::try_from(count)
            .map_err(|_| BinaryError::invalid_data("read length does not fit in u64"))?;
        ByteCursor::read_bytes(self, count)
    }

    fn read_cstring_limited(&mut self, max_len: usize) -> Result<String> {
        let max_len = u64::try_from(max_len)
            .map_err(|_| BinaryError::invalid_data("C string limit does not fit in u64"))?;
        ByteCursor::read_cstring(self, max_len)
    }
}

impl BinaryInput for ByteCursor<'_, '_> {
    fn check_bytes(&self, amount: u64) -> Result<()> {
        ByteCursor::check_bytes(self, amount)
    }

    fn check_entries(&self, amount: u64) -> Result<()> {
        ByteCursor::check_entries(self, amount)
    }

    fn check_members(&self, amount: u64) -> Result<()> {
        ByteCursor::check_members(self, amount)
    }

    fn check_depth(&self, depth: u32) -> Result<()> {
        ByteCursor::check_depth(self, depth)
    }

    fn consume_bytes(&mut self, amount: u64) -> Result<()> {
        ByteCursor::consume_bytes(self, amount)
    }

    fn consume_entries(&mut self, amount: u64) -> Result<()> {
        ByteCursor::consume_entries(self, amount)
    }

    fn consume_members(&mut self, amount: u64) -> Result<()> {
        ByteCursor::consume_members(self, amount)
    }

    fn observe_depth(&mut self, depth: u32) -> Result<()> {
        ByteCursor::observe_depth(self, depth)
    }
}

pub(crate) fn not_enough_data_u64(expected: u64, actual: u64) -> BinaryError {
    match (usize::try_from(expected), usize::try_from(actual)) {
        (Ok(expected), Ok(actual)) => BinaryError::not_enough_data(expected, actual),
        _ => BinaryError::invalid_data(format!(
            "input requires {expected} bytes but only {actual} remain"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_reading() {
        let data = [0x01, 0x02, 0x03, 0x04];
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);

        assert_eq!(reader.read_u8().unwrap(), 0x01);
        assert_eq!(reader.read_u8().unwrap(), 0x02);
        assert_eq!(reader.position(), 2);
        assert_eq!(reader.remaining(), 2);
    }

    #[test]
    fn test_skip_bytes() {
        let data = [0x01, 0x02, 0x03, 0x04, 0x05];
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);

        reader.skip_bytes(2).unwrap();
        assert_eq!(reader.position(), 2);
        assert_eq!(reader.read_u8().unwrap(), 0x03);

        assert!(reader.skip_bytes(10).is_err());
    }

    #[test]
    fn test_endianness() {
        let data = [0x01, 0x02, 0x03, 0x04];

        let mut reader_le = BinaryReader::new(&data, ByteOrder::Little);
        assert_eq!(reader_le.read_u32().unwrap(), 0x04030201);

        let mut reader_be = BinaryReader::new(&data, ByteOrder::Big);
        assert_eq!(reader_be.read_u32().unwrap(), 0x01020304);
    }

    #[test]
    fn test_string_reading() {
        let data = b"Hello\0World\0";
        let mut reader = BinaryReader::new(data, ByteOrder::Little);

        assert_eq!(reader.read_cstring().unwrap(), "Hello");
        assert_eq!(reader.read_cstring().unwrap(), "World");
    }

    #[test]
    fn test_alignment() {
        let data = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);

        reader.read_u8().unwrap(); // pos = 1
        reader.align().unwrap(); // pos = 4
        assert_eq!(reader.position(), 4);
    }
}
