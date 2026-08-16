use unity_asset_binary::reader::ByteOrder;
use unity_asset_core::{Result, UnityAssetError};

/// An in-memory binary writer with UnityPy-like ergonomics.
///
/// This intentionally mirrors `UnityPy.streams.EndianBinaryWriter` semantics:
/// - exposes `Position`-like cursor behavior
/// - writes signed lengths for strings/arrays (Unity style)
/// - supports `align_stream`
#[derive(Debug, Clone)]
pub struct BinaryWriter {
    byte_order: ByteOrder,
    buf: Vec<u8>,
    pos: usize,
    error: Option<String>,
}

impl BinaryWriter {
    pub fn new(byte_order: ByteOrder) -> Self {
        Self {
            byte_order,
            buf: Vec::new(),
            pos: 0,
            error: None,
        }
    }

    pub fn with_bytes(byte_order: ByteOrder, bytes: Vec<u8>) -> Self {
        let pos = bytes.len();
        Self {
            byte_order,
            buf: bytes,
            pos,
            error: None,
        }
    }

    pub const fn byte_order(&self) -> ByteOrder {
        self.byte_order
    }

    pub fn set_byte_order(&mut self, byte_order: ByteOrder) {
        self.byte_order = byte_order;
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn set_position(&mut self, pos: usize) {
        if self.error.is_some() {
            return;
        }
        self.pos = pos;
        if self.pos > self.buf.len() {
            if let Err(error) = self
                .buf
                .try_reserve(self.pos.saturating_sub(self.buf.len()))
            {
                self.record_error(format!("Failed to reserve writer buffer: {error}"));
                return;
            }
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

    /// Returns an error recorded by a checked write or allocation operation.
    pub fn ensure_valid(&self) -> Result<()> {
        match &self.error {
            Some(message) => Err(UnityAssetError::format(message.clone())),
            None => Ok(()),
        }
    }

    /// Finishes the writer only if every checked write succeeded.
    pub fn into_result(self) -> Result<Vec<u8>> {
        if let Some(message) = self.error {
            return Err(UnityAssetError::format(message));
        }
        Ok(self.buf)
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if self.error.is_some() {
            return;
        }
        let Some(end) = self.pos.checked_add(bytes.len()) else {
            self.record_error(format!(
                "Writer position overflow: {} + {}",
                self.pos,
                bytes.len()
            ));
            return;
        };
        if end > self.buf.len() {
            if let Err(error) = self.buf.try_reserve(end - self.buf.len()) {
                self.record_error(format!("Failed to reserve writer buffer: {error}"));
                return;
            }
            self.buf.resize(end, 0);
        }
        self.buf[self.pos..end].copy_from_slice(bytes);
        self.pos = end;
    }

    pub fn align_stream(&mut self, alignment: usize) {
        if alignment == 0 || self.error.is_some() {
            return;
        }
        let pos = self.pos;
        let pad = (alignment - (pos % alignment)) % alignment;
        if pad == 0 {
            return;
        }

        let Some(end) = self.pos.checked_add(pad) else {
            self.record_error(format!("Writer alignment overflow: {} + {pad}", self.pos));
            return;
        };
        if end > self.buf.len() {
            if let Err(error) = self.buf.try_reserve(end - self.buf.len()) {
                self.record_error(format!("Failed to reserve writer buffer: {error}"));
                return;
            }
            self.buf.resize(end, 0);
        } else {
            self.buf[self.pos..end].fill(0);
        }
        self.pos = end;
    }

    fn record_error(&mut self, message: String) {
        if self.error.is_none() {
            self.error = Some(message);
        }
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
        let bytes = match self.byte_order {
            ByteOrder::Little => value.to_le_bytes(),
            ByteOrder::Big => value.to_be_bytes(),
        };
        self.write(&bytes);
    }

    pub fn write_i16(&mut self, value: i16) {
        let bytes = match self.byte_order {
            ByteOrder::Little => value.to_le_bytes(),
            ByteOrder::Big => value.to_be_bytes(),
        };
        self.write(&bytes);
    }

    pub fn write_u32(&mut self, value: u32) {
        let bytes = match self.byte_order {
            ByteOrder::Little => value.to_le_bytes(),
            ByteOrder::Big => value.to_be_bytes(),
        };
        self.write(&bytes);
    }

    pub fn write_i32(&mut self, value: i32) {
        let bytes = match self.byte_order {
            ByteOrder::Little => value.to_le_bytes(),
            ByteOrder::Big => value.to_be_bytes(),
        };
        self.write(&bytes);
    }

    pub fn write_u64(&mut self, value: u64) {
        let bytes = match self.byte_order {
            ByteOrder::Little => value.to_le_bytes(),
            ByteOrder::Big => value.to_be_bytes(),
        };
        self.write(&bytes);
    }

    pub fn write_i64(&mut self, value: i64) {
        let bytes = match self.byte_order {
            ByteOrder::Little => value.to_le_bytes(),
            ByteOrder::Big => value.to_be_bytes(),
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
        Self::new(ByteOrder::Little)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_order_writes_match_expected_bytes() {
        let mut w = BinaryWriter::new(ByteOrder::Big);
        w.write_i32(0x0102_0304);
        w.write_u16(0x0506);
        assert_eq!(w.bytes(), &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);

        let mut w = BinaryWriter::new(ByteOrder::Little);
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
    fn set_position_reports_capacity_overflow_without_allocating() {
        let mut writer = BinaryWriter::default();
        writer.set_position(usize::MAX);

        let error = writer
            .ensure_valid()
            .expect_err("impossible capacity must be reported");
        assert!(
            error
                .to_string()
                .contains("Failed to reserve writer buffer")
        );
        assert!(writer.into_result().is_err());
    }

    #[test]
    fn write_byte_array_prefixes_i32_length() {
        let mut w = BinaryWriter::default();
        w.write_byte_array(&[9, 8, 7]).unwrap();
        assert_eq!(w.bytes(), &[3, 0, 0, 0, 9, 8, 7]);
    }
}
