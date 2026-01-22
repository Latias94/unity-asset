use std::fmt;

/// UnityPy-compatible packer selector for container saving.
///
/// UnityPy accepts:
/// - `"none"` (default)
/// - `"lz4"`
/// - `"lzma"`
/// - `"original"`
/// - a tuple `(block_info_flag, data_flag)` for UnityFS
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnityPyPacker {
    None,
    Lz4,
    Lzma,
    Original,
    UnityFsFlags {
        block_info_flag: u32,
        data_flag: u32,
    },
}

impl UnityPyPacker {
    pub fn from_unitypy_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "lz4" => Some(Self::Lz4),
            "lzma" => Some(Self::Lzma),
            "original" => Some(Self::Original),
            _ => None,
        }
    }
}

impl fmt::Display for UnityPyPacker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnityPyPacker::None => write!(f, "none"),
            UnityPyPacker::Lz4 => write!(f, "lz4"),
            UnityPyPacker::Lzma => write!(f, "lzma"),
            UnityPyPacker::Original => write!(f, "original"),
            UnityPyPacker::UnityFsFlags {
                block_info_flag,
                data_flag,
            } => write!(f, "({block_info_flag}, {data_flag})"),
        }
    }
}

/// Options for saving/repacking outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackerOptions {
    pub packer: UnityPyPacker,
}

impl Default for PackerOptions {
    fn default() -> Self {
        Self {
            packer: UnityPyPacker::None,
        }
    }
}
