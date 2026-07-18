//! Metadata type definitions
//!
//! This module defines all the data structures used for Unity asset metadata extraction.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Comprehensive metadata for a Unity asset
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetMetadata {
    /// Basic file information
    pub file_info: FileInfo,
    /// Object statistics
    pub object_stats: ObjectStatistics,
    /// Asset relationships
    pub relationships: AssetRelationships,
    /// Performance metrics
    pub performance: PerformanceMetrics,
}

/// Basic file information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub file_size: u64,
    pub unity_version: String,
    pub target_platform: String,
    pub compression_type: String,
    pub file_format_version: u32,
}

/// Object statistics within the asset
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ObjectStatistics {
    pub total_objects: usize,
    pub objects_by_type: HashMap<String, usize>,
    pub largest_objects: Vec<ObjectSummary>,
    pub memory_usage: MemoryUsage,
}

/// Summary of an individual object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectSummary {
    pub path_id: i64,
    pub class_name: String,
    pub name: Option<String>,
    pub byte_size: u32,
}

/// Memory usage statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryUsage {
    pub total_bytes: u64,
    pub by_type: HashMap<String, u64>,
    pub largest_type: Option<String>,
    pub average_object_size: f64,
}

impl Default for MemoryUsage {
    fn default() -> Self {
        Self {
            total_bytes: 0,
            by_type: HashMap::new(),
            largest_type: None,
            average_object_size: 0.0,
        }
    }
}

/// Asset relationships and hierarchy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetRelationships {
    pub gameobject_hierarchy: Vec<GameObjectHierarchy>,
    pub component_relationships: Vec<ComponentRelationship>,
}

/// GameObject hierarchy information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameObjectHierarchy {
    pub gameobject_id: i64,
    pub name: String,
    pub parent_id: Option<i64>,
    pub children_ids: Vec<i64>,
    pub transform_id: i64,
    pub components: Vec<i64>,
    pub depth: u32,
}

/// Component relationship information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentRelationship {
    pub component_id: i64,
    pub component_type: String,
    pub gameobject_id: i64,
}

/// Performance metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceMetrics {
    pub parse_time_ms: f64,
    pub memory_peak_mb: f64,
    pub object_parse_rate: f64, // objects per second
    pub complexity_score: f64,
}

/// Metadata extraction configuration
#[derive(Debug, Clone)]
pub struct ExtractionConfig {
    /// Whether to include hierarchy analysis
    pub include_hierarchy: bool,
    /// Maximum number of objects to analyze (0 = no limit)
    pub max_objects: Option<usize>,
    /// Whether to include performance metrics
    pub include_performance: bool,
    /// Whether to extract detailed object summaries
    pub include_object_details: bool,
}

impl Default for ExtractionConfig {
    fn default() -> Self {
        Self {
            include_hierarchy: true,
            max_objects: None,
            include_performance: true,
            include_object_details: true,
        }
    }
}

/// Metadata extraction result
#[derive(Debug, Clone)]
pub struct ExtractionResult {
    pub metadata: AssetMetadata,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl ExtractionResult {
    pub fn new(metadata: AssetMetadata) -> Self {
        Self {
            metadata,
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub fn add_warning(&mut self, warning: String) {
        self.warnings.push(warning);
    }

    pub fn add_error(&mut self, error: String) {
        self.errors.push(error);
    }

    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// Statistics about the extraction process
#[derive(Debug, Clone)]
pub struct ExtractionStats {
    pub objects_processed: usize,
    pub relationships_found: usize,
    pub processing_time_ms: f64,
    pub memory_used_mb: f64,
}

impl Default for ExtractionStats {
    fn default() -> Self {
        Self {
            objects_processed: 0,
            relationships_found: 0,
            processing_time_ms: 0.0,
            memory_used_mb: 0.0,
        }
    }
}

/// Unity class ID constants for metadata extraction (single source of truth: `unity-asset-core`)
pub use unity_asset_core::class_ids;

/// Helper functions for metadata types
impl AssetMetadata {
    /// Create a new empty metadata structure
    pub fn new() -> Self {
        Self {
            file_info: FileInfo {
                file_size: 0,
                unity_version: String::new(),
                target_platform: String::new(),
                compression_type: String::new(),
                file_format_version: 0,
            },
            object_stats: ObjectStatistics::default(),
            relationships: AssetRelationships {
                gameobject_hierarchy: Vec::new(),
                component_relationships: Vec::new(),
            },
            performance: PerformanceMetrics {
                parse_time_ms: 0.0,
                memory_peak_mb: 0.0,
                object_parse_rate: 0.0,
                complexity_score: 0.0,
            },
        }
    }

    /// Get total number of objects
    pub fn total_objects(&self) -> usize {
        self.object_stats.total_objects
    }

    /// Get total memory usage
    pub fn total_memory_bytes(&self) -> u64 {
        self.object_stats.memory_usage.total_bytes
    }

    /// Check if the asset has hierarchy information
    pub fn has_hierarchy(&self) -> bool {
        !self.relationships.gameobject_hierarchy.is_empty()
    }
}

impl Default for AssetMetadata {
    fn default() -> Self {
        Self::new()
    }
}
