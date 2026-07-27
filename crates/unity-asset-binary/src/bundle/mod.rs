//! Unity AssetBundle format model and parser.
//!
//! # Architecture
//!
//! The module is organized into several sub-modules:
//! - `header` - Bundle header parsing and validation
//! - `types` - Core data structures (AssetBundle, BundleFileInfo, etc.)
//! - `compression` - Compression handling (LZ4, LZMA, Brotli)
//! - `parser` - Main parsing logic for different bundle formats
//!
//! Filesystem loading belongs to [`crate::file`]. Callers that already own bytes use
//! [`BundleParser`] with a caller-owned [`unity_asset_core::AssetLoadBudget`].

pub mod compression;
pub mod header;
pub mod parser;
pub mod types;

// Re-export main types for easy access
pub use compression::{BundleCompression, CompressionOptions, CompressionStats};
pub use header::{BundleFormatInfo, BundleHeader, BundleLayoutKind};
pub use parser::{
    BundleBlockInspection, BundleDirectoryInspection, BundleInspection, BundleInspectionStats,
    BundleLegacyInspection,
};
pub use parser::{BundleParser, ParsingComplexity};
pub use types::{AssetBundle, BundleFileInfo, BundleLoadOptions, BundleStatistics, DirectoryNode};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_options() {
        let lazy_options = BundleLoadOptions::lazy();
        assert!(!lazy_options.load_assets);
        assert!(!lazy_options.decompress_blocks);
        assert!(lazy_options.validate);

        let complete_options = BundleLoadOptions::complete();
        assert!(complete_options.load_assets);
        assert!(complete_options.decompress_blocks);
        assert!(complete_options.validate);
    }
}
