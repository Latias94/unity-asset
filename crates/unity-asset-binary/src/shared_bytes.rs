use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum SharedBytes {
    Arc(Arc<[u8]>),
    #[cfg(feature = "mmap")]
    Mmap(Arc<memmap2::Mmap>),
}

impl SharedBytes {
    pub fn from_vec(data: Vec<u8>) -> Self {
        Self::Arc(data.into())
    }

    pub fn from_arc(data: Arc<[u8]>) -> Self {
        Self::Arc(data)
    }

    pub fn as_bytes(&self) -> &[u8] {
        match self {
            SharedBytes::Arc(v) => v.as_ref(),
            #[cfg(feature = "mmap")]
            SharedBytes::Mmap(v) => v.as_ref(),
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
