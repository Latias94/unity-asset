//! Unity asset metadata processing module
//!
//! This module provides comprehensive metadata extraction and analysis capabilities
//! for Unity assets, organized following best practices for maintainability.
//!
//! # Architecture
//!
//! The module is organized into several sub-modules:
//! - `types` - Core data structures for metadata representation
//! - `extractor` - Main metadata extraction functionality
//! - `analyzer` - Advanced dependency and relationship analysis
//!
//! # Examples
//!
//! ```rust,no_run
//! use unity_asset_binary::metadata::{MetadataExtractor, ExtractionConfig};
//! use unity_asset_binary::SerializedFile;
//!
//! // Create extractor with custom configuration
//! let config = ExtractionConfig {
//!     include_dependencies: true,
//!     include_hierarchy: true,
//!     max_objects: Some(1000),
//!     include_performance: true,
//!     include_object_details: true,
//! };
//! let extractor = MetadataExtractor::with_config(config);
//!
//! // Extract metadata from asset
//! let result = extractor.extract_from_asset(&asset)?;
//! println!("Total objects: {}", result.metadata.total_objects());
//! # Ok::<(), unity_asset_binary::error::BinaryError>(())
//! ```

pub mod analyzer;
pub mod extractor;
pub mod types;

// Re-export main types for easy access
pub use analyzer::{DependencyAnalyzer, RelationshipAnalyzer};
pub use extractor::MetadataExtractor;
pub use types::{
    // Core metadata types
    AssetMetadata,
    AssetReference,
    // Relationship types
    AssetRelationships,
    ComponentRelationship,
    DependencyGraph,
    // Dependency types
    DependencyInfo,
    ExternalReference,
    ExtractionConfig,
    ExtractionResult,
    ExtractionStats,
    FileInfo,
    GameObjectHierarchy,
    InternalReference,
    MemoryUsage,
    ObjectStatistics,
    ObjectSummary,
    // Performance and configuration
    PerformanceMetrics,
    // Constants
    class_ids,
};

/// Main metadata processing facade
///
/// This struct provides a high-level interface for metadata processing,
/// combining extraction and analysis functionality.
pub struct MetadataProcessor {
    extractor: MetadataExtractor,
    dependency_analyzer: Option<DependencyAnalyzer>,
    relationship_analyzer: Option<RelationshipAnalyzer>,
}

impl MetadataProcessor {
    /// Create a new metadata processor with default settings
    pub fn new() -> Self {
        Self {
            extractor: MetadataExtractor::new(),
            dependency_analyzer: None,
            relationship_analyzer: None,
        }
    }

    /// Create a metadata processor with custom configuration
    pub fn with_config(config: ExtractionConfig) -> Self {
        let enable_advanced = config.include_dependencies || config.include_hierarchy;

        Self {
            extractor: MetadataExtractor::with_config(config),
            dependency_analyzer: if enable_advanced {
                Some(DependencyAnalyzer::new())
            } else {
                None
            },
            relationship_analyzer: if enable_advanced {
                Some(RelationshipAnalyzer::new())
            } else {
                None
            },
        }
    }

    /// Process metadata from a SerializedFile
    pub fn process_asset(
        &mut self,
        asset: &crate::SerializedFile,
    ) -> crate::error::Result<ExtractionResult> {
        let mut result = self.extractor.extract_from_asset(asset)?;

        // Enhanced dependency analysis if analyzer is available
        if let Some(ref mut analyzer) = self.dependency_analyzer
            && self.extractor.config().include_dependencies {
                let objects: Vec<&crate::asset::ObjectInfo> = asset.objects.iter().collect();
                match analyzer.analyze_dependencies(&objects) {
                    Ok(deps) => {
                        result.metadata.dependencies = deps;
                    }
                    Err(e) => {
                        result.add_warning(format!("Enhanced dependency analysis failed: {}", e));
                    }
                }
            }

        // Enhanced relationship analysis if analyzer is available
        if let Some(ref mut analyzer) = self.relationship_analyzer
            && self.extractor.config().include_hierarchy {
                let objects: Vec<&crate::asset::ObjectInfo> = asset.objects.iter().collect();
                match analyzer.analyze_relationships(&objects) {
                    Ok(rels) => {
                        result.metadata.relationships = rels;
                    }
                    Err(e) => {
                        result.add_warning(format!("Enhanced relationship analysis failed: {}", e));
                    }
                }
            }

        Ok(result)
    }

    /// Process metadata from an AssetBundle
    pub fn process_bundle(
        &mut self,
        bundle: &crate::AssetBundle,
    ) -> crate::error::Result<Vec<ExtractionResult>> {
        let mut results = Vec::new();

        for asset in &bundle.assets {
            let result = self.process_asset(asset)?;
            results.push(result);
        }

        Ok(results)
    }

    /// Get the current extraction configuration
    pub fn config(&self) -> &ExtractionConfig {
        self.extractor.config()
    }

    /// Update the extraction configuration
    pub fn set_config(&mut self, config: ExtractionConfig) {
        let enable_advanced = config.include_dependencies || config.include_hierarchy;

        self.extractor.set_config(config);

        // Initialize analyzers if needed
        if enable_advanced {
            if self.dependency_analyzer.is_none() {
                self.dependency_analyzer = Some(DependencyAnalyzer::new());
            }
            if self.relationship_analyzer.is_none() {
                self.relationship_analyzer = Some(RelationshipAnalyzer::new());
            }
        }
    }

    /// Clear internal caches
    pub fn clear_caches(&mut self) {
        if let Some(ref mut analyzer) = self.dependency_analyzer {
            analyzer.clear_cache();
        }
        if let Some(ref mut analyzer) = self.relationship_analyzer {
            analyzer.clear_cache();
        }
    }

    /// Check if advanced analysis is enabled
    pub fn has_advanced_analysis(&self) -> bool {
        self.dependency_analyzer.is_some() || self.relationship_analyzer.is_some()
    }
}

impl Default for MetadataProcessor {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience functions for common operations
/// Create a metadata processor with default settings
pub fn create_processor() -> MetadataProcessor {
    MetadataProcessor::default()
}

/// Create a metadata processor with performance-focused configuration
pub fn create_performance_processor() -> MetadataProcessor {
    let config = ExtractionConfig {
        include_dependencies: false,
        include_hierarchy: false,
        max_objects: Some(1000),
        include_performance: true,
        include_object_details: false,
    };
    MetadataProcessor::with_config(config)
}

/// Create a metadata processor with comprehensive analysis
pub fn create_comprehensive_processor() -> MetadataProcessor {
    let config = ExtractionConfig {
        include_dependencies: true,
        include_hierarchy: true,
        max_objects: None,
        include_performance: true,
        include_object_details: true,
    };
    MetadataProcessor::with_config(config)
}

/// Extract basic metadata from an asset
pub fn extract_basic_metadata(
    asset: &crate::SerializedFile,
) -> crate::error::Result<AssetMetadata> {
    let extractor = MetadataExtractor::new();
    let result = extractor.extract_from_asset(asset)?;
    Ok(result.metadata)
}

/// Extract metadata with custom configuration
pub fn extract_metadata_with_config(
    asset: &crate::SerializedFile,
    config: ExtractionConfig,
) -> crate::error::Result<ExtractionResult> {
    let extractor = MetadataExtractor::with_config(config);
    extractor.extract_from_asset(asset)
}

/// Get quick statistics for an asset
pub fn get_asset_statistics(asset: &crate::SerializedFile) -> AssetStatistics {
    AssetStatistics {
        object_count: asset.objects.len(),
        type_count: asset.types.len(),
        external_count: asset.externals.len(),
        file_size: asset.header.file_size as u64,
        unity_version: asset.unity_version.clone(),
        format_version: asset.header.version,
    }
}

/// Quick asset statistics
#[derive(Debug, Clone)]
pub struct AssetStatistics {
    pub object_count: usize,
    pub type_count: usize,
    pub external_count: usize,
    pub file_size: u64,
    pub unity_version: String,
    pub format_version: u32,
}

/// Metadata processing options
#[derive(Debug, Clone)]
pub struct ProcessingOptions {
    pub enable_caching: bool,
    pub max_cache_size: usize,
    pub parallel_processing: bool,
    pub memory_limit_mb: Option<usize>,
}

impl Default for ProcessingOptions {
    fn default() -> Self {
        Self {
            enable_caching: true,
            max_cache_size: 1000,
            parallel_processing: false,
            memory_limit_mb: None,
        }
    }
}

/// Check if metadata extraction is supported for an asset
pub fn is_extraction_supported(asset: &crate::SerializedFile) -> bool {
    // Support Unity 5.0+ (version 10+)
    asset.header.version >= 10
}

/// Get recommended extraction configuration for an asset
pub fn get_recommended_config(asset: &crate::SerializedFile) -> ExtractionConfig {
    let object_count = asset.objects.len();

    if object_count > 10000 {
        // Large asset - performance focused
        ExtractionConfig {
            include_dependencies: false,
            include_hierarchy: false,
            max_objects: Some(5000),
            include_performance: true,
            include_object_details: false,
        }
    } else if object_count > 1000 {
        // Medium asset - balanced
        ExtractionConfig {
            include_dependencies: true,
            include_hierarchy: false,
            max_objects: Some(2000),
            include_performance: true,
            include_object_details: true,
        }
    } else {
        // Small asset - comprehensive
        ExtractionConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_processor_creation() {
        let processor = create_processor();
        assert!(!processor.has_advanced_analysis());
    }

    #[test]
    fn test_comprehensive_processor() {
        let processor = create_comprehensive_processor();
        assert!(processor.has_advanced_analysis());
        assert!(processor.config().include_dependencies);
        assert!(processor.config().include_hierarchy);
    }

    #[test]
    fn test_performance_processor() {
        let processor = create_performance_processor();
        assert!(!processor.config().include_dependencies);
        assert!(!processor.config().include_hierarchy);
        assert_eq!(processor.config().max_objects, Some(1000));
    }

    #[test]
    fn test_extraction_support() {
        // This would need a mock SerializedFile for proper testing
        // For now, just test that the function exists
    }
}
