//! Unity Asset Metadata Extraction System
//!
//! This module provides advanced metadata extraction capabilities for Unity assets,
//! including dependency analysis, object statistics, and asset relationships.

use crate::error::Result;
use crate::{AssetBundle, SerializedFile, UnityObject};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Comprehensive metadata for a Unity asset
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetMetadata {
    /// Basic file information
    pub file_info: FileInfo,
    /// Object statistics
    pub object_stats: ObjectStatistics,
    /// Dependency information
    pub dependencies: DependencyInfo,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub dependencies: Vec<i64>,
}

/// Memory usage statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryUsage {
    pub total_bytes: u64,
    pub by_type: HashMap<String, u64>,
    pub largest_type: Option<String>,
    pub average_object_size: f64,
}

/// Dependency information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyInfo {
    pub external_references: Vec<ExternalReference>,
    pub internal_references: Vec<InternalReference>,
    pub dependency_graph: DependencyGraph,
    pub circular_dependencies: Vec<Vec<i64>>,
}

/// External file reference
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalReference {
    pub file_id: i32,
    pub path_id: i64,
    pub referenced_by: Vec<i64>,
}

/// Internal object reference
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InternalReference {
    pub from_object: i64,
    pub to_object: i64,
    pub reference_type: String,
}

/// Dependency graph representation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyGraph {
    pub nodes: Vec<i64>,
    pub edges: Vec<(i64, i64)>,
    pub root_objects: Vec<i64>,
    pub leaf_objects: Vec<i64>,
}

/// Asset relationships and hierarchy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetRelationships {
    pub gameobject_hierarchy: Vec<GameObjectHierarchy>,
    pub component_relationships: Vec<ComponentRelationship>,
    pub asset_references: Vec<AssetReference>,
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
    pub dependencies: Vec<i64>,
}

/// Asset reference information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetReference {
    pub asset_id: i64,
    pub asset_type: String,
    pub referenced_by: Vec<i64>,
    pub file_path: Option<String>,
}

/// Performance metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceMetrics {
    pub parse_time_ms: f64,
    pub memory_peak_mb: f64,
    pub object_parse_rate: f64, // objects per second
    pub complexity_score: f64,
}

/// Metadata extractor for Unity assets
pub struct MetadataExtractor {
    /// Configuration options
    pub include_dependencies: bool,
    pub include_hierarchy: bool,
    pub include_performance: bool,
    pub max_objects_to_analyze: Option<usize>,
}

impl Default for MetadataExtractor {
    fn default() -> Self {
        Self {
            include_dependencies: true,
            include_hierarchy: true,
            include_performance: true,
            max_objects_to_analyze: None,
        }
    }
}

impl MetadataExtractor {
    /// Create a new metadata extractor with default settings
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a metadata extractor with custom settings
    pub fn with_config(
        include_dependencies: bool,
        include_hierarchy: bool,
        include_performance: bool,
        max_objects: Option<usize>,
    ) -> Self {
        Self {
            include_dependencies,
            include_hierarchy,
            include_performance,
            max_objects_to_analyze: max_objects,
        }
    }

    /// Extract metadata from an AssetBundle
    pub fn extract_from_bundle(&self, bundle: &AssetBundle) -> Result<Vec<AssetMetadata>> {
        let start_time = std::time::Instant::now();
        let mut metadata_list = Vec::new();

        for asset in bundle.assets() {
            let metadata = self.extract_from_asset(asset)?;
            metadata_list.push(metadata);
        }

        // Add bundle-level performance metrics
        let total_time = start_time.elapsed().as_secs_f64() * 1000.0;
        let list_len = metadata_list.len() as f64;
        for metadata in &mut metadata_list {
            metadata.performance.parse_time_ms = total_time / list_len;
        }

        Ok(metadata_list)
    }

    /// Extract metadata from a SerializedFile
    pub fn extract_from_asset(&self, asset: &SerializedFile) -> Result<AssetMetadata> {
        let start_time = std::time::Instant::now();

        // Get all objects
        let objects = asset.get_objects()?;
        let objects_to_analyze = if let Some(max) = self.max_objects_to_analyze {
            objects.into_iter().take(max).collect()
        } else {
            objects
        };

        // Extract basic file info
        let file_info = self.extract_file_info(asset);

        // Extract object statistics
        let object_stats = self.extract_object_statistics(&objects_to_analyze);

        // Extract dependencies if enabled
        let dependencies = if self.include_dependencies {
            self.extract_dependencies(&objects_to_analyze)?
        } else {
            DependencyInfo {
                external_references: Vec::new(),
                internal_references: Vec::new(),
                dependency_graph: DependencyGraph {
                    nodes: Vec::new(),
                    edges: Vec::new(),
                    root_objects: Vec::new(),
                    leaf_objects: Vec::new(),
                },
                circular_dependencies: Vec::new(),
            }
        };

        // Extract relationships if enabled
        let relationships = if self.include_hierarchy {
            self.extract_relationships(&objects_to_analyze)?
        } else {
            AssetRelationships {
                gameobject_hierarchy: Vec::new(),
                component_relationships: Vec::new(),
                asset_references: Vec::new(),
            }
        };

        // Calculate performance metrics
        let performance = if self.include_performance {
            let parse_time = start_time.elapsed().as_secs_f64() * 1000.0;
            PerformanceMetrics {
                parse_time_ms: parse_time,
                memory_peak_mb: 0.0, // TODO: Implement memory tracking
                object_parse_rate: objects_to_analyze.len() as f64 / (parse_time / 1000.0),
                complexity_score: self.calculate_complexity_score(&object_stats, &dependencies),
            }
        } else {
            PerformanceMetrics {
                parse_time_ms: 0.0,
                memory_peak_mb: 0.0,
                object_parse_rate: 0.0,
                complexity_score: 0.0,
            }
        };

        Ok(AssetMetadata {
            file_info,
            object_stats,
            dependencies,
            relationships,
            performance,
        })
    }

    /// Extract basic file information
    pub fn extract_file_info(&self, asset: &SerializedFile) -> FileInfo {
        FileInfo {
            file_size: asset.header.file_size as u64,
            unity_version: asset.unity_version().to_string(),
            target_platform: format!("Platform_{}", asset.target_platform()),
            compression_type: "None".to_string(), // TODO: Detect compression
            file_format_version: asset.header.version,
        }
    }

    /// Extract object statistics
    pub fn extract_object_statistics(&self, objects: &[UnityObject]) -> ObjectStatistics {
        let mut objects_by_type: HashMap<String, usize> = HashMap::new();
        let mut memory_by_type: HashMap<String, u64> = HashMap::new();
        let mut total_memory = 0u64;
        let mut object_summaries = Vec::new();

        for obj in objects {
            let class_name = obj.class_name().to_string();

            // Count objects by type
            *objects_by_type.entry(class_name.clone()).or_insert(0) += 1;

            // Sum memory by type
            let byte_size = obj.byte_size() as u64;
            *memory_by_type.entry(class_name.clone()).or_insert(0u64) += byte_size;
            total_memory += byte_size;

            // Create object summary
            object_summaries.push(ObjectSummary {
                path_id: obj.path_id(),
                class_name: class_name.clone(),
                name: obj.name(),
                byte_size: obj.byte_size(),
                dependencies: Vec::new(), // TODO: Extract dependencies
            });
        }

        // Sort by size and keep largest objects
        object_summaries.sort_by(|a, b| b.byte_size.cmp(&a.byte_size));
        let largest_objects = object_summaries.into_iter().take(10).collect();

        // Find largest type by memory
        let largest_type = memory_by_type
            .iter()
            .max_by_key(|(_, size)| *size)
            .map(|(name, _)| name.clone());

        let memory_usage = MemoryUsage {
            total_bytes: total_memory,
            by_type: memory_by_type,
            largest_type,
            average_object_size: if objects.is_empty() {
                0.0
            } else {
                total_memory as f64 / objects.len() as f64
            },
        };

        ObjectStatistics {
            total_objects: objects.len(),
            objects_by_type,
            largest_objects,
            memory_usage,
        }
    }

    /// Extract dependency information
    pub fn extract_dependencies(&self, objects: &[UnityObject]) -> Result<DependencyInfo> {
        let mut external_refs = Vec::new();
        let mut internal_refs = Vec::new();
        let mut all_nodes = HashSet::new();
        let mut edges = Vec::new();

        for obj in objects {
            all_nodes.insert(obj.path_id());

            // Extract references from object properties
            self.extract_object_references(
                obj,
                &mut external_refs,
                &mut internal_refs,
                &mut edges,
            )?;
        }

        // Build dependency graph
        let nodes: Vec<i64> = all_nodes.into_iter().collect();
        let root_objects = self.find_root_objects(&nodes, &edges);
        let leaf_objects = self.find_leaf_objects(&nodes, &edges);

        let dependency_graph = DependencyGraph {
            nodes,
            edges: edges.clone(),
            root_objects,
            leaf_objects,
        };

        // Detect circular dependencies
        let circular_dependencies = self.detect_circular_dependencies(&edges);

        Ok(DependencyInfo {
            external_references: external_refs,
            internal_references: internal_refs,
            dependency_graph,
            circular_dependencies,
        })
    }

    /// Extract object references from properties
    fn extract_object_references(
        &self,
        obj: &UnityObject,
        external_refs: &mut Vec<ExternalReference>,
        internal_refs: &mut Vec<InternalReference>,
        edges: &mut Vec<(i64, i64)>,
    ) -> Result<()> {
        // This is a simplified implementation
        // In a real implementation, we would traverse the object's properties
        // and extract all ObjectRef instances

        // For now, we'll extract references from known object types
        if obj.is_gameobject() {
            if let Ok(gameobject) = obj.as_gameobject() {
                for component_ref in &gameobject.components {
                    if component_ref.file_id == 0 && !component_ref.is_null() {
                        internal_refs.push(InternalReference {
                            from_object: obj.path_id(),
                            to_object: component_ref.path_id,
                            reference_type: "Component".to_string(),
                        });
                        edges.push((obj.path_id(), component_ref.path_id));
                    } else if !component_ref.is_null() {
                        external_refs.push(ExternalReference {
                            file_id: component_ref.file_id,
                            path_id: component_ref.path_id,
                            referenced_by: vec![obj.path_id()],
                        });
                    }
                }
            }
        }

        if obj.is_transform() {
            if let Ok(transform) = obj.as_transform() {
                // Parent reference
                if let Some(parent_ref) = &transform.parent {
                    if parent_ref.file_id == 0 && !parent_ref.is_null() {
                        internal_refs.push(InternalReference {
                            from_object: obj.path_id(),
                            to_object: parent_ref.path_id,
                            reference_type: "Parent".to_string(),
                        });
                        edges.push((obj.path_id(), parent_ref.path_id));
                    }
                }

                // Children references
                for child_ref in &transform.children {
                    if child_ref.file_id == 0 && !child_ref.is_null() {
                        internal_refs.push(InternalReference {
                            from_object: obj.path_id(),
                            to_object: child_ref.path_id,
                            reference_type: "Child".to_string(),
                        });
                        edges.push((obj.path_id(), child_ref.path_id));
                    }
                }
            }
        }

        Ok(())
    }

    /// Extract asset relationships
    pub fn extract_relationships(&self, objects: &[UnityObject]) -> Result<AssetRelationships> {
        let mut gameobject_hierarchy = Vec::new();
        let mut component_relationships = Vec::new();
        let asset_references = Vec::new();

        // Build GameObject hierarchy
        for obj in objects {
            if obj.is_gameobject() {
                if let Ok(gameobject) = obj.as_gameobject() {
                    // Find the Transform component to get hierarchy info
                    let transform_id = gameobject
                        .components
                        .iter()
                        .find(|comp| comp.file_id == 0)
                        .map(|comp| comp.path_id)
                        .unwrap_or(0);

                    gameobject_hierarchy.push(GameObjectHierarchy {
                        gameobject_id: obj.path_id(),
                        name: gameobject.name.clone(),
                        parent_id: None,          // TODO: Extract from Transform
                        children_ids: Vec::new(), // TODO: Extract from Transform
                        transform_id,
                        components: gameobject.components.iter().map(|c| c.path_id).collect(),
                        depth: 0, // TODO: Calculate depth
                    });
                }
            }
        }

        // Build component relationships
        for obj in objects {
            if !obj.is_gameobject() && !obj.is_transform() {
                component_relationships.push(ComponentRelationship {
                    component_id: obj.path_id(),
                    component_type: obj.class_name().to_string(),
                    gameobject_id: 0,         // TODO: Find owning GameObject
                    dependencies: Vec::new(), // TODO: Extract dependencies
                });
            }
        }

        Ok(AssetRelationships {
            gameobject_hierarchy,
            component_relationships,
            asset_references,
        })
    }

    /// Calculate complexity score based on statistics and dependencies
    pub fn calculate_complexity_score(
        &self,
        stats: &ObjectStatistics,
        deps: &DependencyInfo,
    ) -> f64 {
        let object_complexity = stats.total_objects as f64 * 0.1;
        let type_diversity = stats.objects_by_type.len() as f64 * 0.5;
        let dependency_complexity = deps.internal_references.len() as f64 * 0.3;
        let circular_penalty = deps.circular_dependencies.len() as f64 * 2.0;

        object_complexity + type_diversity + dependency_complexity + circular_penalty
    }

    /// Find root objects (no incoming edges)
    fn find_root_objects(&self, nodes: &[i64], edges: &[(i64, i64)]) -> Vec<i64> {
        let targets: HashSet<i64> = edges.iter().map(|(_, to)| *to).collect();
        nodes
            .iter()
            .filter(|&&node| !targets.contains(&node))
            .copied()
            .collect()
    }

    /// Find leaf objects (no outgoing edges)
    fn find_leaf_objects(&self, nodes: &[i64], edges: &[(i64, i64)]) -> Vec<i64> {
        let sources: HashSet<i64> = edges.iter().map(|(from, _)| *from).collect();
        nodes
            .iter()
            .filter(|&&node| !sources.contains(&node))
            .copied()
            .collect()
    }

    /// Detect circular dependencies using DFS
    fn detect_circular_dependencies(&self, _edges: &[(i64, i64)]) -> Vec<Vec<i64>> {
        // Simplified circular dependency detection
        // In a real implementation, we would use a proper cycle detection algorithm
        Vec::new()
    }
}
