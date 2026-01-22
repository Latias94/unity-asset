use unity_asset_core::{Result, UnityAssetError};

/// A writable resource "cab" (Unity `.resS` / `.resource`) buffer.
///
/// This mirrors UnityPy's `get_writeable_cab()` writer behavior at a minimal level:
/// callers append bytes and record offsets/sizes into streamed-resource fields.
#[derive(Debug, Clone)]
pub struct WritableCab {
    pub name: String,
    pub flags: u32,
    bytes: Vec<u8>,
}

impl WritableCab {
    pub fn new(name: impl Into<String>, flags: u32) -> Self {
        Self {
            name: name.into(),
            flags,
            bytes: Vec::new(),
        }
    }

    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    pub fn append(&mut self, data: &[u8]) -> Result<u64> {
        let offset = self.len();
        let new_len = self
            .bytes
            .len()
            .checked_add(data.len())
            .ok_or_else(|| UnityAssetError::format("WritableCab size overflow"))?;
        self.bytes.reserve(new_len.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(data);
        Ok(offset)
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}
