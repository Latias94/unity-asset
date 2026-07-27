//! Allocation-free media layout descriptors.

/// A borrowed reference to Unity stream data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamDataRef<'a> {
    path: &'a str,
    offset: u64,
    size: u32,
}

impl<'a> StreamDataRef<'a> {
    /// Creates a validated reference to non-empty stream data.
    #[must_use]
    pub const fn new(path: &'a str, offset: u64, size: u32) -> Option<Self> {
        if path.is_empty() || size == 0 {
            return None;
        }
        Some(Self { path, offset, size })
    }

    #[must_use]
    pub const fn path(self) -> &'a str {
        self.path
    }

    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }

    #[must_use]
    pub const fn size(self) -> u32 {
        self.size
    }
}

/// The selected encoded payload without owning or copying media bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPayloadRef<'a> {
    /// Media bytes are embedded in the inspected Unity object.
    Embedded { byte_len: usize },
    /// Media bytes live in an external streamed resource.
    Streamed(StreamDataRef<'a>),
}

impl<'a> MediaPayloadRef<'a> {
    #[cfg(any(feature = "audio", feature = "texture"))]
    pub(crate) const fn select(
        embedded_byte_len: usize,
        stream: Option<StreamDataRef<'a>>,
    ) -> Option<Self> {
        if embedded_byte_len != 0 {
            return Some(Self::Embedded {
                byte_len: embedded_byte_len,
            });
        }
        match stream {
            Some(stream) => Some(Self::Streamed(stream)),
            None => None,
        }
    }

    #[must_use]
    pub const fn embedded_byte_len(self) -> Option<usize> {
        match self {
            Self::Embedded { byte_len } => Some(byte_len),
            Self::Streamed(_) => None,
        }
    }

    #[must_use]
    pub const fn stream(self) -> Option<StreamDataRef<'a>> {
        match self {
            Self::Embedded { .. } => None,
            Self::Streamed(stream) => Some(stream),
        }
    }
}

#[cfg(any(feature = "audio", feature = "texture"))]
pub(crate) fn is_plausible_stream_path(path: &str) -> bool {
    path.is_empty()
        || path.contains("archive:/")
        || path.contains('/')
        || path.contains('\\')
        || path.ends_with(".resS")
        || path.ends_with(".resource")
}
