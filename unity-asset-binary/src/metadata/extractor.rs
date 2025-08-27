//! Metadata extraction implementation
//!
//! This module provides the main metadata extraction functionality for Unity assets.

use crate::error::Result;
use crate::{AssetBundle, SerializedFile};
use super::types::*;
use std::collections::HashMap;
use std::time::Instant;

/// Metadata extractor for Unity assets
/// 
/// This struct provides methods for extracting comprehensive metadata
/// from Unity assets including statistics, dependencies, and relationships.
pub struct MetadataExtractor {
    config: ExtractionConfig,
}

impl MetadataExtractor {
    /// Create a new metadata extractor with default settings
    pub fn new() -> Self {
        Self {
            config: ExtractionConfig::default(),
        }
    }

    /// Create a metadata extractor with custom configuration
    pub fn with_config(config: ExtractionConfig) -> Self {
        Self { config }
    }

    /// Create a metadata extractor with custom settings (legacy API)
    pub fn with_settings(
        include_dependencies: bool,
        include_hierarchy: bool,
        include_performance: bool,
        max_objects: Option<usize>,
    ) -> Self {
        Self {
            config: ExtractionConfig {
                include_dependencies,
                include_hierarchy,
                max_objects,
                include_performance,
                include_object_details: true,
            },
        }
    }

    /// Extract metadata from an AssetBundle
    pub fn extract_from_bundle(&self, bundle: &AssetBundle) -> Result<Vec<ExtractionResult>> {
        let start_time = Instant::now();
        let mut results = Vec::new();

        for asset in &bundle.assets {
            let result = self.extract_from_asset(asset)?;
            results.push(result);
        }

        // Add bundle-level performance metrics
        let total_time = start_time.elapsed().as_secs_f64() * 1000.0;
        let asset_count = results.len() as f64;
        
        for result in &mut results {
            result.metadata.performance.parse_time_ms = total_time / asset_count;
        }

        Ok(results)
    }

    /// Extract metadata from a SerializedFile
    pub fn extract_from_asset(&self, asset: &SerializedFile) -> Result<ExtractionResult> {
        let start_time = Instant::now();
        let mut result = ExtractionResult::new(AssetMetadata::new());

        // Get objects to analyze
        let objects_to_analyze: Vec<&crate::asset::ObjectInfo> = if let Some(max) = self.config.max_objects {
            asset.objects.iter().take(max).collect()
        } else {
            asset.objects.iter().collect()
        };

        // Extract basic file info
        result.metadata.file_info = self.extract_file_info(asset);

        // Extract object statistics
        result.metadata.object_stats = self.extract_object_statistics(&objects_to_analyze);

        // Extract dependencies if enabled
        if self.config.include_dependencies {
            match self.extract_dependencies(&objects_to_analyze) {
                Ok(deps) => result.metadata.dependencies = deps,
                Err(e) => {
                    result.add_warning(format!("Failed to extract dependencies: {}", e));
                    result.metadata.dependencies = DependencyInfo {
                        external_references: Vec::new(),
                        internal_references: Vec::new(),
                        dependency_graph: DependencyGraph {
                            nodes: Vec::new(),
                            edges: Vec::new(),
                            root_objects: Vec::new(),
                            leaf_objects: Vec::new(),
                        },
                        circular_dependencies: Vec::new(),
                    };
                }
            }
        }

        // Extract relationships if enabled
        if self.config.include_hierarchy {
            match self.extract_relationships(&objects_to_analyze) {
                Ok(rels) => result.metadata.relationships = rels,
                Err(e) => {
                    result.add_warning(format!("Failed to extract relationships: {}", e));
                    result.metadata.relationships = AssetRelationships {
                        gameobject_hierarchy: Vec::new(),
                        component_relationships: Vec::new(),
                        asset_references: Vec::new(),
                    };
                }
            }
        }

        // Extract performance metrics if enabled
        if self.config.include_performance {
            let elapsed = start_time.elapsed().as_secs_f64() * 1000.0;
            result.metadata.performance = self.extract_performance_metrics(asset, elapsed);
        }

        Ok(result)
    }

    /// Extract basic file information
    fn extract_file_info(&self, asset: &SerializedFile) -> FileInfo {
        FileInfo {
            file_size: asset.header.file_size as u64,
            unity_version: asset.unity_version.clone(),
            target_platform: format!("{}", asset.target_platform),
            compression_type: "None".to_string(), // TODO: Detect compression
            file_format_version: asset.header.version,
        }
    }

    /// Extract object statistics
    fn extract_object_statistics(&self, objects: &[&crate::asset::ObjectInfo]) -> ObjectStatistics {
        let mut objects_by_type: HashMap<String, usize> = HashMap::new();
        let mut memory_by_type: HashMap<String, u64> = HashMap::new();
        let mut total_memory = 0u64;
        let mut object_summaries = Vec::new();

        for obj in objects {
            // Get class name from type_id (simplified mapping)
            let class_name = self.get_class_name_from_type_id(obj.type_id);

            // Count objects by type
            *objects_by_type.entry(class_name.clone()).or_insert(0) += 1;

            // Sum memory by type
            let byte_size = obj.byte_size as u64;
            *memory_by_type.entry(class_name.clone()).or_insert(0u64) += byte_size;
            total_memory += byte_size;

            // Create object summary if detailed extraction is enabled
            if self.config.include_object_details {
                object_summaries.push(ObjectSummary {
                    path_id: obj.path_id,
                    class_name: class_name.clone(),
                    name: Some(format!("Object_{}", obj.path_id)), // Simplified name
                    byte_size: obj.byte_size,
                    dependencies: Vec::new(), // TODO: Extract dependencies
                });
            }
        }

        // Sort by size and keep largest objects
        object_summaries.sort_by(|a, b| b.byte_size.cmp(&a.byte_size));
        if object_summaries.len() > 100 {
            object_summaries.truncate(100); // Keep top 100
        }

        // Find largest type
        let largest_type = memory_by_type
            .iter()
            .max_by_key(|&(_, &size)| size)
            .map(|(name, _)| name.clone());

        // Calculate average object size
        let average_size = if objects.is_empty() {
            0.0
        } else {
            total_memory as f64 / objects.len() as f64
        };

        ObjectStatistics {
            total_objects: objects.len(),
            objects_by_type,
            largest_objects: object_summaries,
            memory_usage: MemoryUsage {
                total_bytes: total_memory,
                by_type: memory_by_type,
                largest_type,
                average_object_size: average_size,
            },
        }
    }

    /// Extract dependency information (simplified implementation)
    fn extract_dependencies(&self, _objects: &[&crate::asset::ObjectInfo]) -> Result<DependencyInfo> {
        // TODO: Implement proper dependency extraction for new ObjectInfo structure
        // This is a placeholder implementation
        Ok(DependencyInfo {
            external_references: Vec::new(),
            internal_references: Vec::new(),
            dependency_graph: DependencyGraph {
                nodes: Vec::new(),
                edges: Vec::new(),
                root_objects: Vec::new(),
                leaf_objects: Vec::new(),
            },
            circular_dependencies: Vec::new(),
        })
    }

    /// Extract relationship information (simplified implementation)
    fn extract_relationships(&self, _objects: &[&crate::asset::ObjectInfo]) -> Result<AssetRelationships> {
        // TODO: Implement proper relationship extraction for new ObjectInfo structure
        // This is a placeholder implementation
        Ok(AssetRelationships {
            gameobject_hierarchy: Vec::new(),
            component_relationships: Vec::new(),
            asset_references: Vec::new(),
        })
    }

    /// Extract performance metrics
    fn extract_performance_metrics(&self, asset: &SerializedFile, parse_time_ms: f64) -> PerformanceMetrics {
        let object_count = asset.objects.len() as f64;
        let object_parse_rate = if parse_time_ms > 0.0 {
            (object_count * 1000.0) / parse_time_ms
        } else {
            0.0
        };

        // Calculate complexity score based on various factors
        let complexity_score = self.calculate_complexity_score(asset);

        PerformanceMetrics {
            parse_time_ms,
            memory_peak_mb: 0.0, // TODO: Implement memory tracking
            object_parse_rate,
            complexity_score,
        }
    }

    /// Calculate complexity score for the asset
    fn calculate_complexity_score(&self, asset: &SerializedFile) -> f64 {
        let object_count = asset.objects.len() as f64;
        let type_count = asset.types.len() as f64;
        let external_count = asset.externals.len() as f64;

        // Simple complexity calculation
        let base_score = object_count * 0.1 + type_count * 0.5 + external_count * 0.3;
        
        // Normalize to 0-100 scale
        (base_score / 100.0).min(100.0)
    }

    /// Get class name from Unity type ID
    fn get_class_name_from_type_id(&self, type_id: i32) -> String {
        match type_id {
            class_ids::GAME_OBJECT => "GameObject".to_string(),
            class_ids::TRANSFORM => "Transform".to_string(),
            class_ids::MATERIAL => "Material".to_string(),
            class_ids::TEXTURE_2D => "Texture2D".to_string(),
            class_ids::MESH => "Mesh".to_string(),
            class_ids::SHADER => "Shader".to_string(),
            class_ids::ANIMATION_CLIP => "AnimationClip".to_string(),
            class_ids::AUDIO_CLIP => "AudioClip".to_string(),
            class_ids::ANIMATOR_CONTROLLER => "AnimatorController".to_string(),
            class_ids::MONO_BEHAVIOUR => "MonoBehaviour".to_string(),
            class_ids::SPRITE => "Sprite".to_string(),
            _ => format!("UnknownType_{}", type_id),
        }
    }

    /// Get the current configuration
    pub fn config(&self) -> &ExtractionConfig {
        &self.config
    }

    /// Update the configuration
    pub fn set_config(&mut self, config: ExtractionConfig) {
        self.config = config;
    }
}

impl Default for MetadataExtractor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extractor_creation() {
        let extractor = MetadataExtractor::new();
        assert!(extractor.config().include_dependencies);
        assert!(extractor.config().include_hierarchy);
    }

    #[test]
    fn test_class_name_mapping() {
        let extractor = MetadataExtractor::new();
        assert_eq!(extractor.get_class_name_from_type_id(1), "GameObject");
        assert_eq!(extractor.get_class_name_from_type_id(28), "Texture2D");
        assert_eq!(extractor.get_class_name_from_type_id(999), "UnknownType_999");
    }

    #[test]
    fn test_complexity_calculation() {
        let _extractor = MetadataExtractor::new();
        // This would need a mock SerializedFile for proper testing
        // For now, just test that the method exists and doesn't panic
    }
}
