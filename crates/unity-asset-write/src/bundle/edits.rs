use std::collections::HashMap;

/// A minimal edit set for repacking a bundle:
/// replace specific directory entries (by name) with new raw bytes.
#[derive(Debug, Clone, Default)]
pub struct BundleEdits {
    by_name: HashMap<String, Vec<u8>>,
    flags: HashMap<String, u32>,
}

impl BundleEdits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn replace_file_bytes(&mut self, name: impl Into<String>, bytes: Vec<u8>) {
        self.by_name.insert(name.into(), bytes);
    }

    pub fn add_file_bytes(&mut self, name: impl Into<String>, bytes: Vec<u8>, flags: u32) {
        let name = name.into();
        self.by_name.insert(name.clone(), bytes);
        self.flags.insert(name, flags);
    }

    pub(crate) fn get(&self, name: &str) -> Option<&[u8]> {
        self.by_name.get(name).map(|v| v.as_slice())
    }

    pub(crate) fn flags(&self, name: &str) -> Option<u32> {
        self.flags.get(name).copied()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.by_name.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }
}
