//! Dependency and relationship analysis
//!
//! This module provides advanced analysis capabilities for Unity assets,
//! including dependency tracking and relationship mapping.

use crate::error::Result;
use super::types::*;
use std::collections::{HashMap, HashSet};

/// Dependency analyzer for Unity assets
/// 
/// This struct provides methods for analyzing dependencies and relationships
/// between Unity objects within and across assets.
pub struct DependencyAnalyzer {
    /// Cache for analyzed dependencies
    dependency_cache: HashMap<i64, Vec<i64>>,
    /// Cache for reverse dependencies
    reverse_dependency_cache: HashMap<i64, Vec<i64>>,
}

impl DependencyAnalyzer {
    /// Create a new dependency analyzer
    pub fn new() -> Self {
        Self {
            dependency_cache: HashMap::new(),
            reverse_dependency_cache: HashMap::new(),
        }
    }

    /// Analyze dependencies for a set of objects
    pub fn analyze_dependencies(&mut self, objects: &[&crate::asset::ObjectInfo]) -> Result<DependencyInfo> {
        let mut external_refs = Vec::new();
        let mut internal_refs = Vec::new();
        let mut all_nodes = HashSet::new();
        let mut edges = Vec::new();

        // First pass: collect all object IDs
        for obj in objects {
            all_nodes.insert(obj.path_id);
        }

        // Second pass: analyze each object's dependencies
        for obj in objects {
            let dependencies = self.extract_object_dependencies(obj)?;
            
            for dep_id in dependencies {
                if all_nodes.contains(&dep_id) {
                    // Internal reference
                    internal_refs.push(InternalReference {
                        from_object: obj.path_id,
                        to_object: dep_id,
                        reference_type: "Direct".to_string(),
                    });
                    edges.push((obj.path_id, dep_id));
                } else {
                    // External reference (simplified)
                    external_refs.push(ExternalReference {
                        file_id: 0, // TODO: Determine actual file ID
                        path_id: dep_id,
                        referenced_by: vec![obj.path_id],
                    });
                }
            }
        }

        // Build dependency graph
        let nodes: Vec<i64> = all_nodes.into_iter().collect();
        let root_objects = self.find_root_objects(&nodes, &edges);
        let leaf_objects = self.find_leaf_objects(&nodes, &edges);
        
        let dependency_graph = DependencyGraph {
            nodes,
            edges,
            root_objects,
            leaf_objects,
        };

        // Detect circular dependencies
        let circular_deps = self.detect_circular_dependencies(&dependency_graph)?;

        Ok(DependencyInfo {
            external_references: external_refs,
            internal_references: internal_refs,
            dependency_graph,
            circular_dependencies: circular_deps,
        })
    }

    /// Extract dependencies from a single object (simplified implementation)
    fn extract_object_dependencies(&mut self, obj: &crate::asset::ObjectInfo) -> Result<Vec<i64>> {
        // Check cache first
        if let Some(cached) = self.dependency_cache.get(&obj.path_id) {
            return Ok(cached.clone());
        }

        // TODO: Implement proper dependency extraction from object data
        // This would require parsing the object's serialized data based on its type
        // For now, return empty dependencies
        let dependencies = Vec::new();

        // Cache the result
        self.dependency_cache.insert(obj.path_id, dependencies.clone());

        Ok(dependencies)
    }

    /// Find root objects (objects with no incoming dependencies)
    fn find_root_objects(&self, nodes: &[i64], edges: &[(i64, i64)]) -> Vec<i64> {
        let mut has_incoming: HashSet<i64> = HashSet::new();
        
        for (_, to) in edges {
            has_incoming.insert(*to);
        }

        nodes.iter()
            .filter(|node| !has_incoming.contains(node))
            .copied()
            .collect()
    }

    /// Find leaf objects (objects with no outgoing dependencies)
    fn find_leaf_objects(&self, nodes: &[i64], edges: &[(i64, i64)]) -> Vec<i64> {
        let mut has_outgoing: HashSet<i64> = HashSet::new();
        
        for (from, _) in edges {
            has_outgoing.insert(*from);
        }

        nodes.iter()
            .filter(|node| !has_outgoing.contains(node))
            .copied()
            .collect()
    }

    /// Detect circular dependencies using DFS
    fn detect_circular_dependencies(&self, graph: &DependencyGraph) -> Result<Vec<Vec<i64>>> {
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();
        let mut cycles = Vec::new();

        // Build adjacency list
        let mut adj_list: HashMap<i64, Vec<i64>> = HashMap::new();
        for node in &graph.nodes {
            adj_list.insert(*node, Vec::new());
        }
        for (from, to) in &graph.edges {
            adj_list.get_mut(from).unwrap().push(*to);
        }

        // DFS for each unvisited node
        for &node in &graph.nodes {
            if !visited.contains(&node) {
                let mut path = Vec::new();
                self.dfs_detect_cycle(
                    node,
                    &adj_list,
                    &mut visited,
                    &mut rec_stack,
                    &mut path,
                    &mut cycles,
                );
            }
        }

        Ok(cycles)
    }

    /// DFS helper for cycle detection
    fn dfs_detect_cycle(
        &self,
        node: i64,
        adj_list: &HashMap<i64, Vec<i64>>,
        visited: &mut HashSet<i64>,
        rec_stack: &mut HashSet<i64>,
        path: &mut Vec<i64>,
        cycles: &mut Vec<Vec<i64>>,
    ) {
        visited.insert(node);
        rec_stack.insert(node);
        path.push(node);

        if let Some(neighbors) = adj_list.get(&node) {
            for &neighbor in neighbors {
                if !visited.contains(&neighbor) {
                    self.dfs_detect_cycle(neighbor, adj_list, visited, rec_stack, path, cycles);
                } else if rec_stack.contains(&neighbor) {
                    // Found a cycle
                    if let Some(cycle_start) = path.iter().position(|&x| x == neighbor) {
                        let cycle = path[cycle_start..].to_vec();
                        cycles.push(cycle);
                    }
                }
            }
        }

        path.pop();
        rec_stack.remove(&node);
    }

    /// Clear internal caches
    pub fn clear_cache(&mut self) {
        self.dependency_cache.clear();
        self.reverse_dependency_cache.clear();
    }

    /// Get cached dependencies for an object
    pub fn get_cached_dependencies(&self, object_id: i64) -> Option<&Vec<i64>> {
        self.dependency_cache.get(&object_id)
    }
}

impl Default for DependencyAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

/// Relationship analyzer for Unity assets
/// 
/// This struct provides methods for analyzing relationships between
/// GameObjects, Components, and other Unity objects.
pub struct RelationshipAnalyzer {
    /// Cache for GameObject hierarchies
    hierarchy_cache: HashMap<i64, GameObjectHierarchy>,
}

impl RelationshipAnalyzer {
    /// Create a new relationship analyzer
    pub fn new() -> Self {
        Self {
            hierarchy_cache: HashMap::new(),
        }
    }

    /// Analyze relationships for a set of objects
    pub fn analyze_relationships(&mut self, objects: &[&crate::asset::ObjectInfo]) -> Result<AssetRelationships> {
        let mut gameobject_hierarchy = Vec::new();
        let mut component_relationships = Vec::new();
        let mut asset_references = Vec::new();

        // Separate objects by type
        let mut gameobjects = Vec::new();
        let mut transforms = Vec::new();
        let mut components = Vec::new();
        let mut assets = Vec::new();

        for obj in objects {
            match obj.type_id {
                class_ids::GAME_OBJECT => gameobjects.push(obj),
                class_ids::TRANSFORM => transforms.push(obj),
                class_ids::COMPONENT | class_ids::BEHAVIOUR | class_ids::MONO_BEHAVIOUR => {
                    components.push(obj)
                }
                _ => assets.push(obj),
            }
        }

        // Analyze GameObject hierarchy (simplified for now)
        for go in gameobjects {
            let hierarchy = GameObjectHierarchy {
                gameobject_id: go.path_id,
                name: format!("GameObject_{}", go.path_id),
                parent_id: None,
                children_ids: Vec::new(),
                transform_id: 0,
                components: Vec::new(),
                depth: 0,
            };
            gameobject_hierarchy.push(hierarchy);
        }

        // Analyze component relationships
        for comp in components {
            if let Ok(relationship) = self.analyze_component_relationship(comp) {
                component_relationships.push(relationship);
            }
        }

        // Analyze asset references
        for asset in assets {
            if let Ok(reference) = self.analyze_asset_reference(asset) {
                asset_references.push(reference);
            }
        }

        Ok(AssetRelationships {
            gameobject_hierarchy,
            component_relationships,
            asset_references,
        })
    }

    /// Analyze GameObject hierarchy (simplified implementation)
    fn analyze_gameobject_hierarchy(
        &mut self,
        gameobject: &crate::asset::ObjectInfo,
        _transforms: &Vec<&crate::asset::ObjectInfo>,
    ) -> Result<GameObjectHierarchy> {
        // TODO: Implement proper GameObject hierarchy analysis
        // This would require parsing the GameObject's serialized data
        
        Ok(GameObjectHierarchy {
            gameobject_id: gameobject.path_id,
            name: format!("GameObject_{}", gameobject.path_id),
            parent_id: None,
            children_ids: Vec::new(),
            transform_id: 0, // TODO: Find associated Transform
            components: Vec::new(),
            depth: 0,
        })
    }

    /// Analyze component relationship (simplified implementation)
    fn analyze_component_relationship(
        &self,
        component: &crate::asset::ObjectInfo,
    ) -> Result<ComponentRelationship> {
        // TODO: Implement proper component relationship analysis
        
        Ok(ComponentRelationship {
            component_id: component.path_id,
            component_type: self.get_component_type_name(component.type_id),
            gameobject_id: 0, // TODO: Find associated GameObject
            dependencies: Vec::new(),
        })
    }

    /// Analyze asset reference (simplified implementation)
    fn analyze_asset_reference(
        &self,
        asset: &crate::asset::ObjectInfo,
    ) -> Result<AssetReference> {
        // TODO: Implement proper asset reference analysis
        
        Ok(AssetReference {
            asset_id: asset.path_id,
            asset_type: self.get_asset_type_name(asset.type_id),
            referenced_by: Vec::new(),
            file_path: None,
        })
    }

    /// Get component type name from type ID
    fn get_component_type_name(&self, type_id: i32) -> String {
        match type_id {
            class_ids::TRANSFORM => "Transform".to_string(),
            class_ids::MONO_BEHAVIOUR => "MonoBehaviour".to_string(),
            _ => format!("Component_{}", type_id),
        }
    }

    /// Get asset type name from type ID
    fn get_asset_type_name(&self, type_id: i32) -> String {
        match type_id {
            class_ids::TEXTURE_2D => "Texture2D".to_string(),
            class_ids::MESH => "Mesh".to_string(),
            class_ids::MATERIAL => "Material".to_string(),
            class_ids::AUDIO_CLIP => "AudioClip".to_string(),
            class_ids::SPRITE => "Sprite".to_string(),
            _ => format!("Asset_{}", type_id),
        }
    }

    /// Clear internal caches
    pub fn clear_cache(&mut self) {
        self.hierarchy_cache.clear();
    }
}

impl Default for RelationshipAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dependency_analyzer_creation() {
        let analyzer = DependencyAnalyzer::new();
        assert!(analyzer.dependency_cache.is_empty());
    }

    #[test]
    fn test_relationship_analyzer_creation() {
        let analyzer = RelationshipAnalyzer::new();
        assert!(analyzer.hierarchy_cache.is_empty());
    }

    #[test]
    fn test_root_leaf_detection() {
        let analyzer = DependencyAnalyzer::new();
        let nodes = vec![1, 2, 3, 4];
        let edges = vec![(1, 2), (2, 3), (4, 3)];
        
        let roots = analyzer.find_root_objects(&nodes, &edges);
        let leaves = analyzer.find_leaf_objects(&nodes, &edges);
        
        assert!(roots.contains(&1));
        assert!(roots.contains(&4));
        assert!(leaves.contains(&3));
    }
}
