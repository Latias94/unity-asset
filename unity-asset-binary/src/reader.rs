//! Binary data reader for Unity files

use crate::error::{BinaryError, Result};
use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use std::io::{Cursor, Read, Seek, SeekFrom};

/// Byte order for reading binary data
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ByteOrder {
    /// Big endian (network byte order)
    Big,
    /// Little endian (most common on x86/x64)
    #[default]
    Little,
}

/// Binary reader for Unity file formats
pub struct BinaryReader<'a> {
    cursor: Cursor<&'a [u8]>,
    byte_order: ByteOrder,
}

impl<'a> BinaryReader<'a> {
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
        let pos = self.position();
        let aligned = (pos + alignment - 1) & !(alignment - 1);
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

    /// Read all remaining bytes
    pub fn read_remaining(&mut self) -> &[u8] {
        let pos = self.cursor.position() as usize;
        let data = self.cursor.get_ref();
        &data[pos..]
    }

    /// Read a null-terminated string
    pub fn read_cstring(&mut self) -> Result<String> {
        let mut bytes = Vec::new();
        loop {
            let byte = self.read_u8()?;
            if byte == 0 {
                break;
            }
            bytes.push(byte);
        }
        Ok(String::from_utf8(bytes)?)
    }

    /// Read a string with a length prefix (32-bit)
    pub fn read_string(&mut self) -> Result<String> {
        let length = self.read_u32()? as usize;
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
