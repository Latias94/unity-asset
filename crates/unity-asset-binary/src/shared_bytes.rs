use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct SharedBytes {
    backing: SharedBytesBacking,
}

#[derive(Debug, Clone)]
enum SharedBytesBacking {
    Arc(Arc<[u8]>),
    OwnedVec(Arc<Vec<u8>>),
    #[cfg(feature = "mmap")]
    Mmap(Arc<memmap2::Mmap>),
}

impl SharedBytes {
    pub fn from_vec(data: Vec<u8>) -> Self {
        Self {
            backing: SharedBytesBacking::OwnedVec(Arc::new(data)),
        }
    }

    pub fn from_arc(data: Arc<[u8]>) -> Self {
        Self {
            backing: SharedBytesBacking::Arc(data),
        }
    }

    #[cfg(feature = "mmap")]
    pub fn from_mmap(data: Arc<memmap2::Mmap>) -> Self {
        Self {
            backing: SharedBytesBacking::Mmap(data),
        }
    }

    /// Returns the canonical slice backing when this value was created from an `Arc<[u8]>`.
    ///
    /// Owned vectors and memory maps intentionally remain opaque so callers cannot couple their
    /// authority or identity rules to the physical backing representation.
    pub fn as_arc_slice(&self) -> Option<&Arc<[u8]>> {
        match &self.backing {
            SharedBytesBacking::Arc(bytes) => Some(bytes),
            SharedBytesBacking::OwnedVec(_) => None,
            #[cfg(feature = "mmap")]
            SharedBytesBacking::Mmap(_) => None,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        match &self.backing {
            SharedBytesBacking::Arc(bytes) => bytes.as_ref(),
            SharedBytesBacking::OwnedVec(bytes) => bytes.as_slice(),
            #[cfg(feature = "mmap")]
            SharedBytesBacking::Mmap(bytes) => bytes.as_ref(),
        }
    }

    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn ptr_usize(&self) -> usize {
        self.as_bytes().as_ptr() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_vec_preserves_the_original_byte_allocation() {
        let bytes = vec![1_u8, 2, 3, 4];
        let original = bytes.as_ptr();

        let shared = SharedBytes::from_vec(bytes);

        assert_eq!(shared.as_bytes(), &[1, 2, 3, 4]);
        assert_eq!(shared.as_bytes().as_ptr(), original);
        assert!(shared.as_arc_slice().is_none());
    }
}
