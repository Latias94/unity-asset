use unity_asset_core::{Result, UnityAssetError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Endian {
    Big,
    #[default]
    Little,
}

/// An in-memory binary writer with UnityPy-like ergonomics.
///
/// This intentionally mirrors `UnityPy.streams.EndianBinaryWriter` semantics:
/// - exposes `Position`-like cursor behavior
/// - writes signed lengths for strings/arrays (Unity style)
/// - supports `align_stream`
#[derive(Debug, Clone)]
pub struct BinaryWriter {
    endian: Endian,
    buf: Vec<u8>,
    pos: usize,
}

impl BinaryWriter {
    pub fn new(endian: Endian) -> Self {
        Self {
            endian,
            buf: Vec::new(),
            pos: 0,
        }
    }

    pub fn with_bytes(endian: Endian, bytes: Vec<u8>) -> Self {
        let pos = bytes.len();
        Self {
            endian,
            buf: bytes,
            pos,
        }
    }

    pub fn endian(&self) -> Endian {
        self.endian
    }

    pub fn set_endian(&mut self, endian: Endian) {
        self.endian = endian;
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn set_position(&mut self, pos: usize) {
        self.pos = pos;
        if self.pos > self.buf.len() {
            self.buf.resize(self.pos, 0);
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn bytes(&self) -> &[u8] {
        self.buf.as_slice()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let end = self.pos.saturating_add(bytes.len());
        if end > self.buf.len() {
            self.buf.resize(end, 0);
        }
        self.buf[self.pos..end].copy_from_slice(bytes);
        self.pos = end;
    }

    pub fn align_stream(&mut self, alignment: usize) {
        if alignment == 0 {
            return;
        }
        let pos = self.pos;
        let pad = (alignment - (pos % alignment)) % alignment;
        if pad == 0 {
            return;
        }

        let end = self.pos.saturating_add(pad);
        if end > self.buf.len() {
            self.buf.resize(end, 0);
        } else {
            self.buf[self.pos..end].fill(0);
        }
        self.pos = end;
    }

    pub fn write_u8(&mut self, value: u8) {
        self.write(&[value]);
    }

    pub fn write_i8(&mut self, value: i8) {
        self.write_u8(value as u8);
    }

    pub fn write_bool(&mut self, value: bool) {
        self.write_u8(if value { 1 } else { 0 });
    }

    pub fn write_u16(&mut self, value: u16) {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.write(&bytes);
    }

    pub fn write_i16(&mut self, value: i16) {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.write(&bytes);
    }

    pub fn write_u32(&mut self, value: u32) {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.write(&bytes);
    }

    pub fn write_i32(&mut self, value: i32) {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.write(&bytes);
    }

    pub fn write_u64(&mut self, value: u64) {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.write(&bytes);
    }

    pub fn write_i64(&mut self, value: i64) {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.write(&bytes);
    }

    pub fn write_f32(&mut self, value: f32) {
        self.write_u32(value.to_bits());
    }

    pub fn write_f64(&mut self, value: f64) {
        self.write_u64(value.to_bits());
    }

    pub fn write_string_to_null(&mut self, value: &str) {
        self.write(value.as_bytes());
        self.write_u8(0);
    }

    pub fn write_aligned_string(&mut self, value: &str) -> Result<()> {
        let bytes = value.as_bytes();
        let len: i32 = bytes.len().try_into().map_err(|_| {
            UnityAssetError::format(format!("String too large for i32 length: {}", bytes.len()))
        })?;
        self.write_i32(len);
        self.write(bytes);
        self.align_stream(4);
        Ok(())
    }

    pub fn write_byte_array(&mut self, value: &[u8]) -> Result<()> {
        let len: i32 = value.len().try_into().map_err(|_| {
            UnityAssetError::format(format!(
                "Byte array too large for i32 length: {}",
                value.len()
            ))
        })?;
        self.write_i32(len);
        self.write(value);
        Ok(())
    }

    pub fn write_array<T, F>(&mut self, values: &[T], write_length: bool, mut f: F) -> Result<()>
    where
        F: FnMut(&mut Self, &T) -> Result<()>,
    {
        if write_length {
            let len: i32 = values.len().try_into().map_err(|_| {
                UnityAssetError::format(format!("Array too large for i32 length: {}", values.len()))
            })?;
            self.write_i32(len);
        }
        for v in values {
            f(self, v)?;
        }
        Ok(())
    }
}

impl Default for BinaryWriter {
    fn default() -> Self {
        Self::new(Endian::Little)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endian_writes_match_expected_bytes() {
        let mut w = BinaryWriter::new(Endian::Big);
        w.write_i32(0x0102_0304);
        w.write_u16(0x0506);
        assert_eq!(w.bytes(), &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);

        let mut w = BinaryWriter::new(Endian::Little);
        w.write_i32(0x0102_0304);
        w.write_u16(0x0506);
        assert_eq!(w.bytes(), &[0x04, 0x03, 0x02, 0x01, 0x06, 0x05]);
    }

    #[test]
    fn align_stream_pads_with_zeros() {
        let mut w = BinaryWriter::default();
        w.write_u8(0xAA);
        w.align_stream(4);
        assert_eq!(w.bytes(), &[0xAA, 0x00, 0x00, 0x00]);
        assert_eq!(w.position(), 4);
    }

    #[test]
    fn write_aligned_string_matches_unitypy_shape() {
        let mut w = BinaryWriter::default();
        w.write_aligned_string("abc").unwrap();
        assert_eq!(w.bytes(), &[3, 0, 0, 0, b'a', b'b', b'c', 0]);
    }

    #[test]
    fn set_position_beyond_end_inserts_zeros() {
        let mut w = BinaryWriter::default();
        w.write_u8(1);
        w.set_position(4);
        w.write_u8(2);
        assert_eq!(w.bytes(), &[1, 0, 0, 0, 2]);
    }

    #[test]
    fn write_byte_array_prefixes_i32_length() {
        let mut w = BinaryWriter::default();
        w.write_byte_array(&[9, 8, 7]).unwrap();
        assert_eq!(w.bytes(), &[3, 0, 0, 0, 9, 8, 7]);
    }
}
