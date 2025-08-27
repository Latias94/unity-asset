//! Unity AssetBundle processing module
//!
//! This module provides comprehensive AssetBundle processing capabilities,
//! organized following UnityPy and unity-rs best practices.
//!
//! # Architecture
//!
//! The module is organized into several sub-modules:
//! - `header` - Bundle header parsing and validation
//! - `types` - Core data structures (AssetBundle, BundleFileInfo, etc.)
//! - `compression` - Compression handling (LZ4, LZMA, Brotli)
//! - `parser` - Main parsing logic for different bundle formats
//! - `loader` - Resource loading and management
//!
//! # Examples
//!
//! ```rust,no_run
//! use unity_asset_binary::bundle::{BundleLoader, BundleLoadOptions};
//!
//! // Simple bundle loading
//! let bundle = unity_asset_binary::bundle::load_bundle("example.bundle")?;
//! println!("Loaded bundle with {} assets", bundle.asset_count());
//!
//! // Advanced loading with options
//! let mut loader = BundleLoader::with_options(BundleLoadOptions::fast());
//! let bundle = loader.load_from_file("example.bundle")?;
//!
//! // Find specific assets
//! let texture_assets = loader.find_assets_by_name("texture");
//! # Ok::<(), unity_asset_binary::error::BinaryError>(())
//! ```

pub mod compression;
pub mod header;
pub mod loader;
pub mod parser;
pub mod types;

// Re-export main types for easy access
pub use compression::{BundleCompression, CompressionOptions, CompressionStats};
pub use header::{BundleFormatInfo, BundleHeader};
pub use loader::{
    BundleLoader, BundleResourceManager, LoaderStatistics, load_bundle, load_bundle_from_memory,
    load_bundle_with_options,
};
pub use parser::{BundleParser, ParsingComplexity};
pub use types::{AssetBundle, BundleFileInfo, BundleLoadOptions, BundleStatistics, DirectoryNode};

#[cfg(feature = "async")]
pub use loader::load_bundle_async;

/// Main bundle processing facade
///
/// This struct provides a high-level interface for bundle processing,
/// combining parsing, loading, and resource management functionality.
pub struct BundleProcessor {
    loader: BundleLoader,
}

impl BundleProcessor {
    /// Create a new bundle processor
    pub fn new() -> Self {
        Self {
            loader: BundleLoader::new(),
        }
    }

    /// Create a new bundle processor with options
    pub fn with_options(options: BundleLoadOptions) -> Self {
        Self {
            loader: BundleLoader::with_options(options),
        }
    }

    /// Load and process a bundle from file
    pub fn process_file<P: AsRef<std::path::Path>>(
        &mut self,
        path: P,
    ) -> crate::error::Result<&AssetBundle> {
        self.loader.load_from_file(path)
    }

    /// Load and process a bundle from memory
    pub fn process_memory(
        &mut self,
        name: String,
        data: Vec<u8>,
    ) -> crate::error::Result<&AssetBundle> {
        self.loader.load_from_memory(name, data)
    }

    /// Get the underlying loader
    pub fn loader(&self) -> &BundleLoader {
        &self.loader
    }

    /// Get mutable access to the underlying loader
    pub fn loader_mut(&mut self) -> &mut BundleLoader {
        &mut self.loader
    }

    /// Extract all assets from a bundle
    pub fn extract_all_assets(&self, bundle_name: &str) -> Option<Vec<&crate::asset::Asset>> {
        self.loader
            .get_bundle(bundle_name)
            .map(|bundle| bundle.assets.iter().collect())
    }

    /// Extract assets by type
    pub fn extract_assets_by_type(
        &self,
        bundle_name: &str,
        _type_id: i32,
    ) -> Option<Vec<&crate::asset::Asset>> {
        self.loader
            .get_bundle(bundle_name)
            .map(|bundle| bundle.assets.iter().collect()) // TODO: Implement proper type filtering
    }

    /// Get bundle information
    pub fn get_bundle_info(&self, bundle_name: &str) -> Option<BundleInfo> {
        self.loader.get_bundle(bundle_name).map(|bundle| {
            let stats = bundle.statistics();
            BundleInfo {
                name: bundle_name.to_string(),
                format: bundle.header.signature.clone(),
                version: bundle.header.version,
                unity_version: bundle.header.unity_version.clone(),
                size: bundle.size(),
                compressed: bundle.is_compressed(),
                file_count: bundle.file_count(),
                asset_count: bundle.asset_count(),
                compression_ratio: stats.compression_ratio,
            }
        })
    }

    /// Validate all loaded bundles
    pub fn validate_all(&self) -> crate::error::Result<()> {
        self.loader.validate_all()
    }

    /// Get processing statistics
    pub fn statistics(&self) -> LoaderStatistics {
        self.loader.get_statistics()
    }
}

impl Default for BundleProcessor {
    fn default() -> Self {
        Self::new()
    }
}

/// Bundle information summary
#[derive(Debug, Clone)]
pub struct BundleInfo {
    pub name: String,
    pub format: String,
    pub version: u32,
    pub unity_version: String,
    pub size: u64,
    pub compressed: bool,
    pub file_count: usize,
    pub asset_count: usize,
    pub compression_ratio: f64,
}

/// Convenience functions for common operations
/// Create a bundle processor with default settings
pub fn create_processor() -> BundleProcessor {
    BundleProcessor::default()
}

/// Quick function to get bundle information
pub fn get_bundle_info<P: AsRef<std::path::Path>>(path: P) -> crate::error::Result<BundleInfo> {
    let data = std::fs::read(&path).map_err(|e| {
        crate::error::BinaryError::generic(format!("Failed to read bundle file: {}", e))
    })?;

    let complexity = BundleParser::estimate_complexity(&data)?;
    let bundle = BundleParser::from_bytes(data)?;
    let stats = bundle.statistics();

    Ok(BundleInfo {
        name: path
            .as_ref()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string(),
        format: complexity.format,
        version: bundle.header.version,
        unity_version: bundle.header.unity_version.clone(),
        size: bundle.size(),
        compressed: complexity.has_compression,
        file_count: bundle.file_count(),
        asset_count: bundle.asset_count(),
        compression_ratio: stats.compression_ratio,
    })
}

/// Quick function to list bundle contents
pub fn list_bundle_contents<P: AsRef<std::path::Path>>(
    path: P,
) -> crate::error::Result<Vec<String>> {
    let bundle = load_bundle(path)?;
    Ok(bundle
        .file_names()
        .into_iter()
        .map(|s| s.to_string())
        .collect())
}

/// Quick function to extract a specific file from bundle
pub fn extract_file_from_bundle<P: AsRef<std::path::Path>>(
    bundle_path: P,
    file_name: &str,
) -> crate::error::Result<Vec<u8>> {
    let bundle = load_bundle(bundle_path)?;

    if let Some(file_info) = bundle.find_file(file_name) {
        bundle.extract_file_data(file_info)
    } else {
        Err(crate::error::BinaryError::generic(format!(
            "File '{}' not found in bundle",
            file_name
        )))
    }
}

/// Check if a file is a valid Unity bundle
pub fn is_valid_bundle<P: AsRef<std::path::Path>>(path: P) -> bool {
    match std::fs::read(path) {
        Ok(data) => {
            if data.len() < 20 {
                return false;
            }

            // Check for known bundle signatures
            let signature = String::from_utf8_lossy(&data[..8]);
            matches!(signature.as_ref(), "UnityFS\0" | "UnityWeb" | "UnityRaw")
        }
        Err(_) => false,
    }
}

/// Get supported bundle formats
pub fn get_supported_formats() -> Vec<&'static str> {
    vec!["UnityFS", "UnityWeb", "UnityRaw"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_processor_creation() {
        let processor = create_processor();
        assert_eq!(processor.statistics().bundle_count, 0);
    }

    #[test]
    fn test_supported_formats() {
        let formats = get_supported_formats();
        assert!(formats.contains(&"UnityFS"));
        assert!(formats.contains(&"UnityWeb"));
        assert!(formats.contains(&"UnityRaw"));
    }

    #[test]
    fn test_bundle_info_structure() {
        // Test that BundleInfo can be created
        let info = BundleInfo {
            name: "test".to_string(),
            format: "UnityFS".to_string(),
            version: 6,
            unity_version: "2019.4.0f1".to_string(),
            size: 1024,
            compressed: true,
            file_count: 5,
            asset_count: 10,
            compression_ratio: 0.7,
        };

        assert_eq!(info.name, "test");
        assert_eq!(info.format, "UnityFS");
        assert!(info.compressed);
    }

    #[test]
    fn test_load_options() {
        let fast_options = BundleLoadOptions::fast();
        assert!(!fast_options.load_assets);
        assert!(!fast_options.decompress_blocks);
        assert!(!fast_options.validate);

        let complete_options = BundleLoadOptions::complete();
        assert!(complete_options.load_assets);
        assert!(complete_options.decompress_blocks);
        assert!(complete_options.validate);
    }
}
