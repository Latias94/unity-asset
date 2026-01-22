use std::collections::HashMap;

use unity_asset_binary::asset::FileIdentifier;

/// In-memory edits to apply when saving a `SerializedFile`.
///
/// This mirrors UnityPy's model where edited objects store overridden raw bytes and the file is
/// later rebuilt.
#[derive(Debug, Default, Clone)]
pub struct SerializedFileEdits {
    /// `path_id -> raw object bytes`
    pub object_bytes: HashMap<i64, Vec<u8>>,
    /// Additional external file identifiers to append when saving.
    pub additional_externals: Vec<FileIdentifier>,
}

impl SerializedFileEdits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_object_bytes(&mut self, path_id: i64, bytes: Vec<u8>) {
        self.object_bytes.insert(path_id, bytes);
    }

    pub fn get(&self, path_id: i64) -> Option<&[u8]> {
        self.object_bytes.get(&path_id).map(|v| v.as_slice())
    }

    pub fn add_external(&mut self, external: FileIdentifier) {
        self.additional_externals.push(external);
    }

    pub fn is_empty(&self) -> bool {
        self.object_bytes.is_empty() && self.additional_externals.is_empty()
    }
}
