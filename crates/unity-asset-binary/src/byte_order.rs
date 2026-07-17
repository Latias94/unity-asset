//! Byte-order vocabulary shared by contiguous and random-access readers.

/// Byte order for reading binary data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ByteOrder {
    /// Big endian (network byte order).
    Big,
    /// Little endian (most common on x86/x64).
    #[default]
    Little,
}
