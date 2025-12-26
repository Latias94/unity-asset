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
//! use unity_asset_binary::asset::{SerializedFile, SerializedFileHeader};
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
//! // Note: In real usage, you would load a SerializedFile from actual data
//! // For demonstration, we'll just show the extractor creation
//! println!("Extractor created with config");
//! println!("Metadata extracted successfully");
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
    ExternalObjectRef,
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

use crate::asset::SerializedFile;
use crate::bundle::AssetBundle;

/// Main metadata processing facade
///
/// This struct provides a high-level interface for metadata processing,
/// combining extraction and analysis functionality.
pub struct MetadataProcessor {
    extractor: MetadataExtractor,
    dependency_analyzer: Option<DependencyAnalyzer>,
    relationship_analyzer: Option<RelationshipAnalyzer>,
}

fn apply_dependency_info_to_relationships(
    dependencies: &DependencyInfo,
    relationships: &mut AssetRelationships,
) {
    let mut by_from: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
    for r in &dependencies.internal_references {
        by_from.entry(r.from_object).or_default().push(r.to_object);
    }
    for v in by_from.values_mut() {
        v.sort_unstable();
        v.dedup();
    }

    let mut by_from_external: std::collections::HashMap<i64, Vec<ExternalObjectRef>> =
        std::collections::HashMap::new();
    for r in &dependencies.external_references {
        let ext = ExternalObjectRef {
            file_id: r.file_id,
            path_id: r.path_id,
            file_path: r.file_path.clone(),
            guid: r.guid,
        };
        for from in &r.referenced_by {
            by_from_external.entry(*from).or_default().push(ext.clone());
        }
    }
    for v in by_from_external.values_mut() {
        v.sort_by_key(|e| (e.file_id, e.path_id));
        v.dedup_by_key(|e| (e.file_id, e.path_id));
    }

    for rel in &mut relationships.component_relationships {
        rel.dependencies = by_from.get(&rel.component_id).cloned().unwrap_or_default();
        rel.external_dependencies = by_from_external
            .get(&rel.component_id)
            .cloned()
            .unwrap_or_default();
    }

    // Build asset reference summary (internal targets + external targets).
    let gameobject_ids: std::collections::HashSet<i64> = relationships
        .gameobject_hierarchy
        .iter()
        .map(|h| h.gameobject_id)
        .collect();
    let component_ids: std::collections::HashSet<i64> = relationships
        .component_relationships
        .iter()
        .map(|c| c.component_id)
        .collect();

    let mut referenced_by_internal: std::collections::HashMap<i64, Vec<i64>> =
        std::collections::HashMap::new();
    for r in &dependencies.internal_references {
        referenced_by_internal
            .entry(r.to_object)
            .or_default()
            .push(r.from_object);
    }
    for v in referenced_by_internal.values_mut() {
        v.sort_unstable();
        v.dedup();
    }

    let mut refs: Vec<AssetReference> = Vec::new();

    for (asset_id, referenced_by) in referenced_by_internal {
        let asset_type = if gameobject_ids.contains(&asset_id) {
            "GameObject".to_string()
        } else if component_ids.contains(&asset_id) {
            "Component".to_string()
        } else {
            "Object".to_string()
        };
        refs.push(AssetReference {
            asset_id,
            asset_type,
            referenced_by,
            file_path: None,
        });
    }

    for r in &dependencies.external_references {
        refs.push(AssetReference {
            asset_id: r.path_id,
            asset_type: format!("ExternalObject(file_id={})", r.file_id),
            referenced_by: r.referenced_by.clone(),
            file_path: r.file_path.clone(),
        });
    }

    refs.sort_by(|a, b| {
        // Prefer refs with known file path, then by ref count desc, then asset_id asc.
        match (b.file_path.is_some()).cmp(&a.file_path.is_some()) {
            std::cmp::Ordering::Equal => {}
            ord => return ord,
        }
        match b.referenced_by.len().cmp(&a.referenced_by.len()) {
            std::cmp::Ordering::Equal => {}
            ord => return ord,
        }
        a.asset_id.cmp(&b.asset_id)
    });

    relationships.asset_references = refs;
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
        asset: &SerializedFile,
    ) -> crate::error::Result<ExtractionResult> {
        let mut result = self.extractor.extract_from_asset(asset)?;

        // Enhanced dependency analysis if analyzer is available
        if let Some(ref mut analyzer) = self.dependency_analyzer
            && self.extractor.config().include_dependencies
            && result.metadata.dependencies.dependency_graph.nodes.is_empty()
        {
            let objects: Vec<&crate::asset::ObjectInfo> =
                if let Some(max) = self.extractor.config().max_objects {
                    asset.objects.iter().take(max).collect()
                } else {
                    asset.objects.iter().collect()
                };

            match analyzer.analyze_dependencies_in_asset(asset, &objects) {
                Ok(deps) => {
                    if self.extractor.config().include_object_details {
                        let mut by_from: std::collections::HashMap<i64, Vec<i64>> =
                            std::collections::HashMap::new();
                        for r in &deps.internal_references {
                            by_from.entry(r.from_object).or_default().push(r.to_object);
                        }
                        for v in by_from.values_mut() {
                            v.sort_unstable();
                            v.dedup();
                        }
                        for summary in &mut result.metadata.object_stats.largest_objects {
                            summary.dependencies =
                                by_from.get(&summary.path_id).cloned().unwrap_or_default();
                        }
                    }
                    result.metadata.dependencies = deps;
                }
                Err(e) => {
                    result.add_warning(format!("Enhanced dependency analysis failed: {}", e));
                }
            }
        }

        // Enhanced relationship analysis if analyzer is available
        if let Some(ref mut analyzer) = self.relationship_analyzer
            && self.extractor.config().include_hierarchy
            && result.metadata.relationships.gameobject_hierarchy.is_empty()
            && result.metadata.relationships.component_relationships.is_empty()
            && result.metadata.relationships.asset_references.is_empty()
        {
            let objects: Vec<&crate::asset::ObjectInfo> =
                if let Some(max) = self.extractor.config().max_objects {
                    asset.objects.iter().take(max).collect()
                } else {
                    asset.objects.iter().collect()
                };

            match analyzer.analyze_relationships_in_asset(asset, &objects) {
                Ok(mut rels) => {
                    if self.extractor.config().include_dependencies {
                        apply_dependency_info_to_relationships(
                            &result.metadata.dependencies,
                            &mut rels,
                        );
                    }
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
        bundle: &AssetBundle,
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
pub fn extract_basic_metadata(asset: &SerializedFile) -> crate::error::Result<AssetMetadata> {
    let mut processor = MetadataProcessor::with_config(ExtractionConfig::default());
    let result = processor.process_asset(asset)?;
    Ok(result.metadata)
}

/// Extract metadata with custom configuration
pub fn extract_metadata_with_config(
    asset: &SerializedFile,
    config: ExtractionConfig,
) -> crate::error::Result<ExtractionResult> {
    let mut processor = MetadataProcessor::with_config(config);
    processor.process_asset(asset)
}

/// Get quick statistics for an asset
pub fn get_asset_statistics(asset: &SerializedFile) -> AssetStatistics {
    AssetStatistics {
        object_count: asset.objects.len(),
        type_count: asset.types.len(),
        external_count: asset.externals.len(),
        file_size: asset.header.file_size,
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
pub fn is_extraction_supported(asset: &SerializedFile) -> bool {
    // Support Unity 5.0+ (version 10+)
    asset.header.version >= 10
}

/// Get recommended extraction configuration for an asset
pub fn get_recommended_config(asset: &SerializedFile) -> ExtractionConfig {
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
mod processor_tests {
    use super::*;

    #[test]
    fn test_apply_dependency_info_to_relationships_fills_component_dependencies() {
        let dependencies = DependencyInfo {
            external_references: vec![ExternalReference {
                file_id: 2,
                path_id: 999,
                referenced_by: vec![11],
                file_path: Some("library/external.assets".to_string()),
                guid: Some([7u8; 16]),
            }],
            internal_references: vec![
                InternalReference {
                    from_object: 10,
                    to_object: 1,
                    reference_type: "Direct".to_string(),
                },
                InternalReference {
                    from_object: 10,
                    to_object: 2,
                    reference_type: "Direct".to_string(),
                },
                InternalReference {
                    from_object: 11,
                    to_object: 3,
                    reference_type: "Direct".to_string(),
                },
            ],
            dependency_graph: DependencyGraph {
                nodes: Vec::new(),
                edges: Vec::new(),
                root_objects: Vec::new(),
                leaf_objects: Vec::new(),
            },
            circular_dependencies: Vec::new(),
        };

        let mut relationships = AssetRelationships {
            gameobject_hierarchy: Vec::new(),
            component_relationships: vec![
                ComponentRelationship {
                    component_id: 10,
                    component_type: "Transform".to_string(),
                    gameobject_id: 100,
                    dependencies: Vec::new(),
                    external_dependencies: Vec::new(),
                },
                ComponentRelationship {
                    component_id: 11,
                    component_type: "MeshRenderer".to_string(),
                    gameobject_id: 100,
                    dependencies: Vec::new(),
                    external_dependencies: Vec::new(),
                },
                ComponentRelationship {
                    component_id: 12,
                    component_type: "Unknown".to_string(),
                    gameobject_id: 100,
                    dependencies: Vec::new(),
                    external_dependencies: Vec::new(),
                },
            ],
            asset_references: Vec::new(),
        };

        apply_dependency_info_to_relationships(&dependencies, &mut relationships);

        assert_eq!(
            relationships.component_relationships[0].dependencies,
            vec![1, 2]
        );
        assert_eq!(
            relationships.component_relationships[1].dependencies,
            vec![3]
        );
        assert_eq!(
            relationships.component_relationships[2].dependencies,
            Vec::<i64>::new()
        );

        assert_eq!(
            relationships.component_relationships[0]
                .external_dependencies
                .len(),
            0
        );
        assert_eq!(
            relationships.component_relationships[1]
                .external_dependencies
                .len(),
            1
        );
        assert_eq!(
            relationships.component_relationships[1].external_dependencies[0],
            ExternalObjectRef {
                file_id: 2,
                path_id: 999,
                file_path: Some("library/external.assets".to_string()),
                guid: Some([7u8; 16]),
            }
        );

        let ext_ref = relationships
            .asset_references
            .iter()
            .find(|r| r.asset_id == 999)
            .expect("external asset reference exists");
        assert_eq!(ext_ref.asset_type, "ExternalObject(file_id=2)");
        assert_eq!(
            ext_ref.file_path,
            Some("library/external.assets".to_string())
        );
        assert_eq!(ext_ref.referenced_by, vec![11]);

        let internal_ref_1 = relationships
            .asset_references
            .iter()
            .find(|r| r.asset_id == 1)
            .expect("internal asset reference 1 exists");
        assert_eq!(internal_ref_1.file_path, None);
        assert_eq!(internal_ref_1.referenced_by, vec![10]);

        let internal_ref_3 = relationships
            .asset_references
            .iter()
            .find(|r| r.asset_id == 3)
            .expect("internal asset reference 3 exists");
        assert_eq!(internal_ref_3.referenced_by, vec![11]);
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
