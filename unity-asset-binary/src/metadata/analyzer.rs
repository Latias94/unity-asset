//! Dependency and relationship analysis
//!
//! This module provides advanced analysis capabilities for Unity assets,
//! including dependency tracking and relationship mapping.

use super::types::*;
use crate::asset::SerializedFile;
use crate::error::Result;
use crate::reader::BinaryReader;
use crate::typetree::{TypeTree, TypeTreeSerializer};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use unity_asset_core::UnityValue;

/// Dependency analyzer for Unity assets
///
/// This struct provides methods for analyzing dependencies and relationships
/// between Unity objects within and across assets.
pub struct DependencyAnalyzer {
    /// Cache for analyzed dependencies
    dependency_cache: HashMap<i64, Vec<i64>>,
    /// Cache for analyzed dependencies (TypeTree + PPtr scan), keyed by (asset identity, path_id)
    pptr_dependency_cache: HashMap<(usize, i64), ExtractedDependencies>,
    /// Cache for reverse dependencies
    reverse_dependency_cache: HashMap<i64, Vec<i64>>,
}

#[derive(Debug, Clone, Default)]
struct ExtractedDependencies {
    internal: Vec<i64>,
    external: Vec<(i32, i64)>,
}

impl DependencyAnalyzer {
    /// Create a new dependency analyzer
    pub fn new() -> Self {
        Self {
            dependency_cache: HashMap::new(),
            pptr_dependency_cache: HashMap::new(),
            reverse_dependency_cache: HashMap::new(),
        }
    }

    /// Analyze dependencies for a set of objects
    ///
    /// Note: this legacy API is a placeholder and returns no dependencies.
    /// Use `analyze_dependencies_in_asset` for real TypeTree-based scanning.
    pub fn analyze_dependencies(
        &mut self,
        objects: &[&crate::asset::ObjectInfo],
    ) -> Result<DependencyInfo> {
        let mut internal_refs = Vec::new();
        let mut all_nodes = HashSet::new();
        let mut edges = Vec::new();

        // First pass: collect all object IDs
        for obj in objects {
            all_nodes.insert(obj.path_id);
        }

        // Placeholder implementation (kept for backward compatibility).
        // Previously this always returned empty dependencies.
        let _ = &mut internal_refs;
        let _ = &mut edges;

        let external_refs = Vec::new();

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

    /// Analyze dependencies for a set of objects within a specific asset.
    ///
    /// This parses object data with TypeTree (when available) and scans for PPtr references
    /// (`fileID`/`pathID` pairs) to build a dependency graph.
    pub fn analyze_dependencies_in_asset(
        &mut self,
        asset: &SerializedFile,
        objects: &[&crate::asset::ObjectInfo],
    ) -> Result<DependencyInfo> {
        let mut external_ref_map: HashMap<(i32, i64), Vec<i64>> = HashMap::new();
        let mut internal_refs = Vec::new();
        let mut all_nodes = HashSet::new();
        let mut edges = Vec::new();

        for obj in objects {
            all_nodes.insert(obj.path_id);
        }

        for obj in objects {
            let deps = self.extract_object_dependencies_in_asset(asset, obj)?;

            for dep_id in deps.internal {
                if all_nodes.contains(&dep_id) {
                    internal_refs.push(InternalReference {
                        from_object: obj.path_id,
                        to_object: dep_id,
                        reference_type: "Direct".to_string(),
                    });
                    edges.push((obj.path_id, dep_id));
                } else {
                    external_ref_map
                        .entry((0, dep_id))
                        .or_default()
                        .push(obj.path_id);
                }
            }

            for (file_id, path_id) in deps.external {
                external_ref_map
                    .entry((file_id, path_id))
                    .or_default()
                    .push(obj.path_id);
            }
        }

        let external_refs = external_ref_map
            .into_iter()
            .map(|((file_id, path_id), mut referenced_by)| {
                referenced_by.sort_unstable();
                referenced_by.dedup();
                let (file_path, guid) = resolve_external_file(asset, file_id);
                ExternalReference {
                    file_id,
                    path_id,
                    referenced_by,
                    file_path,
                    guid,
                }
            })
            .collect();

        let nodes: Vec<i64> = all_nodes.into_iter().collect();
        let root_objects = self.find_root_objects(&nodes, &edges);
        let leaf_objects = self.find_leaf_objects(&nodes, &edges);

        let dependency_graph = DependencyGraph {
            nodes,
            edges,
            root_objects,
            leaf_objects,
        };

        let circular_deps = self.detect_circular_dependencies(&dependency_graph)?;

        Ok(DependencyInfo {
            external_references: external_refs,
            internal_references: internal_refs,
            dependency_graph,
            circular_dependencies: circular_deps,
        })
    }

    /// Extract dependencies from a single object by parsing its TypeTree and scanning PPtr-like fields.
    fn extract_object_dependencies_in_asset(
        &mut self,
        asset: &SerializedFile,
        obj: &crate::asset::ObjectInfo,
    ) -> Result<ExtractedDependencies> {
        let data = asset.data_arc();
        let file_key = Arc::as_ptr(&data) as *const u8 as usize;

        if let Some(cached) = self.pptr_dependency_cache.get(&(file_key, obj.path_id)) {
            return Ok(cached.clone());
        }

        let mut deps = ExtractedDependencies::default();

        if asset.enable_type_tree {
            if let Some(tree) = type_tree_for_object(asset, obj) {
                if !tree.is_empty() {
                    // Prefer a zero-allocation scan that still consumes the object stream
                    // according to the TypeTree. This keeps dependency analysis fast even for
                    // large objects with big buffers/arrays.
                    if let Ok(scanned) = scan_object_pptrs_with_typetree(asset, obj, tree) {
                        deps = scanned;
                    } else if let Ok(values) = parse_object_with_typetree(asset, obj, tree) {
                        // Fallback: legacy full parse + recursive scan.
                        scan_pptr_in_value(&UnityValue::Object(values), &mut deps);
                    }
                }
            }
        }

        deps.internal.sort_unstable();
        deps.internal.dedup();
        deps.external.sort_unstable();
        deps.external.dedup();

        self.pptr_dependency_cache
            .insert((file_key, obj.path_id), deps.clone());

        Ok(deps)
    }

    /// Find root objects (objects with no incoming dependencies)
    fn find_root_objects(&self, nodes: &[i64], edges: &[(i64, i64)]) -> Vec<i64> {
        let mut has_incoming: HashSet<i64> = HashSet::new();

        for (_, to) in edges {
            has_incoming.insert(*to);
        }

        nodes
            .iter()
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

        nodes
            .iter()
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
                Self::dfs_detect_cycle(
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
                    Self::dfs_detect_cycle(neighbor, adj_list, visited, rec_stack, path, cycles);
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
        self.pptr_dependency_cache.clear();
        self.reverse_dependency_cache.clear();
    }

    /// Get cached dependencies for an object
    pub fn get_cached_dependencies(&self, object_id: i64) -> Option<&Vec<i64>> {
        self.dependency_cache.get(&object_id)
    }

    /// Get cached TypeTree-based dependencies for an object within an asset.
    pub fn get_cached_dependencies_in_asset(
        &self,
        asset: &SerializedFile,
        object_id: i64,
    ) -> Option<(Vec<i64>, Vec<(i32, i64)>)> {
        let data = asset.data_arc();
        let file_key = Arc::as_ptr(&data) as *const u8 as usize;
        self.pptr_dependency_cache
            .get(&(file_key, object_id))
            .map(|deps| (deps.internal.clone(), deps.external.clone()))
    }
}

impl Default for DependencyAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

fn type_tree_for_object<'a>(
    asset: &'a SerializedFile,
    info: &crate::asset::ObjectInfo,
) -> Option<&'a TypeTree> {
    if info.type_index >= 0 {
        return asset
            .types
            .get(info.type_index as usize)
            .map(|t| &t.type_tree);
    }

    asset
        .types
        .iter()
        .find(|t| t.class_id == info.type_id)
        .map(|t| &t.type_tree)
}

fn parse_object_with_typetree(
    asset: &SerializedFile,
    info: &crate::asset::ObjectInfo,
    tree: &TypeTree,
) -> Result<indexmap::IndexMap<String, UnityValue>> {
    let bytes = asset.object_bytes(info)?;
    let mut reader = BinaryReader::new(bytes, asset.header.byte_order());
    let serializer = TypeTreeSerializer::new(tree);
    if asset.ref_types.is_empty() {
        serializer.parse_object(&mut reader)
    } else {
        serializer.parse_object_with_ref_types(&mut reader, &asset.ref_types)
    }
}

fn scan_object_pptrs_with_typetree(
    asset: &SerializedFile,
    info: &crate::asset::ObjectInfo,
    tree: &TypeTree,
) -> Result<ExtractedDependencies> {
    let bytes = asset.object_bytes(info)?;
    let mut reader = BinaryReader::new(bytes, asset.header.byte_order());
    let serializer = TypeTreeSerializer::new(tree);
    let scan = serializer.scan_pptrs(&mut reader)?;

    let mut deps = ExtractedDependencies::default();
    deps.internal = scan.internal;
    deps.external = scan.external;
    Ok(deps)
}

fn resolve_external_file(
    asset: &SerializedFile,
    file_id: i32,
) -> (Option<String>, Option<[u8; 16]>) {
    if file_id <= 0 {
        return (None, None);
    }

    let idx = (file_id - 1) as usize;
    let Some(ext) = asset.externals.get(idx) else {
        return (None, None);
    };

    let file_path = if ext.path.is_empty() {
        None
    } else {
        Some(ext.path.clone())
    };
    let guid = Some(ext.guid);
    (file_path, guid)
}

fn scan_pptr_in_value(value: &UnityValue, deps: &mut ExtractedDependencies) {
    match value {
        UnityValue::Array(items) => {
            for item in items {
                scan_pptr_in_value(item, deps);
            }
        }
        UnityValue::Object(obj) => {
            if let Some((file_id, path_id)) = try_read_pptr(obj) {
                if path_id != 0 {
                    if file_id == 0 {
                        deps.internal.push(path_id);
                    } else {
                        deps.external.push((file_id, path_id));
                    }
                }
            }

            for (_, v) in obj.iter() {
                scan_pptr_in_value(v, deps);
            }
        }
        _ => {}
    }
}

fn try_read_pptr(map: &indexmap::IndexMap<String, UnityValue>) -> Option<(i32, i64)> {
    let file_id = get_i32_ci(map, &["fileID", "m_FileID"])?;
    let path_id = get_i64_ci(map, &["pathID", "m_PathID"])?;
    Some((file_id, path_id))
}

fn extract_gameobject_components(props: &indexmap::IndexMap<String, UnityValue>) -> Vec<i64> {
    let Some(UnityValue::Array(items)) = props.get("m_Component") else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for item in items {
        match item {
            UnityValue::Object(obj) => {
                // Unity typetree usually stores { "component": {fileID, pathID} }.
                if let Some(UnityValue::Object(component_obj)) = obj.get("component") {
                    if let Some((file_id, path_id)) = try_read_pptr(component_obj) {
                        if file_id == 0 && path_id != 0 {
                            out.push(path_id);
                        }
                    }
                    continue;
                }

                // Fallback: treat the object itself as PPtr if it matches.
                if let Some((file_id, path_id)) = try_read_pptr(obj) {
                    if file_id == 0 && path_id != 0 {
                        out.push(path_id);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn extract_transform_gameobject(props: &indexmap::IndexMap<String, UnityValue>) -> Option<i64> {
    let value = props.get("m_GameObject")?;
    extract_internal_path_id(value)
}

fn extract_transform_parent(props: &indexmap::IndexMap<String, UnityValue>) -> Option<i64> {
    let value = props.get("m_Father")?;
    extract_internal_path_id(value)
}

fn extract_transform_children(props: &indexmap::IndexMap<String, UnityValue>) -> Vec<i64> {
    let Some(UnityValue::Array(items)) = props.get("m_Children") else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for item in items {
        if let Some(path_id) = extract_internal_path_id(item) {
            if path_id != 0 {
                out.push(path_id);
            }
        }
    }
    out
}

fn extract_internal_path_id(value: &UnityValue) -> Option<i64> {
    match value {
        UnityValue::Object(obj) => {
            let (file_id, path_id) = try_read_pptr(obj)?;
            if file_id == 0 { Some(path_id) } else { None }
        }
        _ => None,
    }
}

fn get_i32_ci(map: &indexmap::IndexMap<String, UnityValue>, keys: &[&str]) -> Option<i32> {
    for key in keys {
        for (k, v) in map.iter() {
            if k.eq_ignore_ascii_case(key) {
                return match v {
                    UnityValue::Integer(i) => Some(*i as i32),
                    UnityValue::Float(f) => Some(*f as i32),
                    _ => None,
                };
            }
        }
    }
    None
}

fn get_i64_ci(map: &indexmap::IndexMap<String, UnityValue>, keys: &[&str]) -> Option<i64> {
    for key in keys {
        for (k, v) in map.iter() {
            if k.eq_ignore_ascii_case(key) {
                return match v {
                    UnityValue::Integer(i) => Some(*i),
                    UnityValue::Float(f) => Some(*f as i64),
                    _ => None,
                };
            }
        }
    }
    None
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
    pub fn analyze_relationships(
        &mut self,
        objects: &[&crate::asset::ObjectInfo],
    ) -> Result<AssetRelationships> {
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

    /// Analyze relationships for a set of objects within a specific asset.
    ///
    /// This method parses GameObject/Transform data via TypeTree (when available) to build:
    /// - GameObject hierarchy (parent/children/depth)
    /// - Component relationships (GameObject -> Component)
    pub fn analyze_relationships_in_asset(
        &mut self,
        asset: &SerializedFile,
        objects: &[&crate::asset::ObjectInfo],
    ) -> Result<AssetRelationships> {
        if !asset.enable_type_tree {
            return self.analyze_relationships(objects);
        }

        let mut by_path_id: HashMap<i64, &crate::asset::ObjectInfo> = HashMap::new();
        for obj in objects {
            by_path_id.insert(obj.path_id, *obj);
        }

        let mut gameobject_props: HashMap<i64, indexmap::IndexMap<String, UnityValue>> =
            HashMap::new();
        let mut transform_props: HashMap<i64, indexmap::IndexMap<String, UnityValue>> =
            HashMap::new();

        for obj in objects {
            match obj.type_id {
                class_ids::GAME_OBJECT => {
                    if let Some(tree) = type_tree_for_object(asset, obj) {
                        if !tree.is_empty() {
                            if let Ok(values) = parse_object_with_typetree(asset, obj, tree) {
                                gameobject_props.insert(obj.path_id, values);
                            }
                        }
                    }
                }
                class_ids::TRANSFORM => {
                    if let Some(tree) = type_tree_for_object(asset, obj) {
                        if !tree.is_empty() {
                            if let Ok(values) = parse_object_with_typetree(asset, obj, tree) {
                                transform_props.insert(obj.path_id, values);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Parse GameObject -> components
        let mut go_name: HashMap<i64, String> = HashMap::new();
        let mut go_components: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut go_transform: HashMap<i64, i64> = HashMap::new();

        for (go_id, props) in &gameobject_props {
            let name = props
                .get("m_Name")
                .and_then(|v| match v {
                    UnityValue::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| format!("GameObject_{}", go_id));
            go_name.insert(*go_id, name);

            let components = extract_gameobject_components(props);
            if !components.is_empty() {
                go_components.insert(*go_id, components.clone());

                // Heuristic: the Transform component (class_id=4) is the GameObject's Transform.
                for component_id in components {
                    if let Some(info) = by_path_id.get(&component_id) {
                        if info.type_id == class_ids::TRANSFORM {
                            go_transform.insert(*go_id, component_id);
                            break;
                        }
                    }
                }
            } else {
                go_components.insert(*go_id, Vec::new());
            }
        }

        // Parse Transform -> (gameobject, parent, children)
        let mut transform_to_go: HashMap<i64, i64> = HashMap::new();
        let mut transform_parent: HashMap<i64, i64> = HashMap::new();
        let mut transform_children: HashMap<i64, Vec<i64>> = HashMap::new();

        for (tr_id, props) in &transform_props {
            if let Some(go_id) = extract_transform_gameobject(props) {
                transform_to_go.insert(*tr_id, go_id);
                go_transform.entry(go_id).or_insert(*tr_id);
            }

            if let Some(parent_id) = extract_transform_parent(props) {
                transform_parent.insert(*tr_id, parent_id);
            }

            let children = extract_transform_children(props);
            if !children.is_empty() {
                transform_children.insert(*tr_id, children);
            }
        }

        // Build GameObject hierarchy entries
        let mut hierarchies: HashMap<i64, GameObjectHierarchy> = HashMap::new();
        for go_id in gameobject_props.keys() {
            let transform_id = go_transform.get(go_id).copied().unwrap_or(0);
            let parent_id = if transform_id != 0 {
                transform_parent
                    .get(&transform_id)
                    .and_then(|pid| transform_to_go.get(pid))
                    .copied()
            } else {
                None
            };

            let mut children_ids = Vec::new();
            if transform_id != 0 {
                if let Some(children) = transform_children.get(&transform_id) {
                    for child_tr in children {
                        if let Some(child_go) = transform_to_go.get(child_tr) {
                            children_ids.push(*child_go);
                        }
                    }
                }
            }
            children_ids.sort_unstable();
            children_ids.dedup();

            let mut comps = go_components.get(go_id).cloned().unwrap_or_default();
            comps.sort_unstable();
            comps.dedup();

            hierarchies.insert(
                *go_id,
                GameObjectHierarchy {
                    gameobject_id: *go_id,
                    name: go_name
                        .get(go_id)
                        .cloned()
                        .unwrap_or_else(|| format!("GameObject_{}", go_id)),
                    parent_id,
                    children_ids,
                    transform_id,
                    components: comps,
                    depth: 0,
                },
            );
        }

        // Compute depth (BFS from roots)
        let mut roots: Vec<i64> = Vec::new();
        for (id, h) in &hierarchies {
            match h.parent_id {
                None => roots.push(*id),
                Some(pid) if !hierarchies.contains_key(&pid) => roots.push(*id),
                _ => {}
            }
        }
        roots.sort_unstable();
        roots.dedup();

        let mut queue: std::collections::VecDeque<(i64, u32)> = std::collections::VecDeque::new();
        for r in roots {
            queue.push_back((r, 0));
        }
        let mut visited: HashSet<i64> = HashSet::new();
        while let Some((node, depth)) = queue.pop_front() {
            if !visited.insert(node) {
                continue;
            }
            if let Some(entry) = hierarchies.get_mut(&node) {
                entry.depth = depth;
                for child in entry.children_ids.clone() {
                    queue.push_back((child, depth.saturating_add(1)));
                }
            }
        }

        // Build component relationships
        let mut component_relationships = Vec::new();
        for (go_id, comp_ids) in &go_components {
            for comp_id in comp_ids {
                let component_type = by_path_id
                    .get(comp_id)
                    .map(|info| self.get_component_type_name(info.type_id))
                    .unwrap_or_else(|| format!("Component_{}", comp_id));

                component_relationships.push(ComponentRelationship {
                    component_id: *comp_id,
                    component_type,
                    gameobject_id: *go_id,
                    dependencies: Vec::new(),
                    external_dependencies: Vec::new(),
                });
            }
        }

        // We still keep asset references as placeholder for now.
        Ok(AssetRelationships {
            gameobject_hierarchy: hierarchies.into_values().collect(),
            component_relationships,
            asset_references: Vec::new(),
        })
    }

    /// Analyze GameObject hierarchy (simplified implementation)
    #[allow(dead_code)]
    fn analyze_gameobject_hierarchy(
        &mut self,
        gameobject: &crate::asset::ObjectInfo,
        _transforms: &[&crate::asset::ObjectInfo],
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
            external_dependencies: Vec::new(),
        })
    }

    /// Analyze asset reference (simplified implementation)
    fn analyze_asset_reference(&self, asset: &crate::asset::ObjectInfo) -> Result<AssetReference> {
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
    use indexmap::IndexMap;

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

    #[test]
    fn test_scan_pptr_variants() {
        let mut deps = ExtractedDependencies::default();

        // Internal reference: fileID=0
        let mut pptr_internal = IndexMap::new();
        pptr_internal.insert("fileID".to_string(), UnityValue::Integer(0));
        pptr_internal.insert("pathID".to_string(), UnityValue::Integer(123));

        // External reference: fileID=2
        let mut pptr_external = IndexMap::new();
        pptr_external.insert("m_FileID".to_string(), UnityValue::Integer(2));
        pptr_external.insert("m_PathID".to_string(), UnityValue::Integer(999));

        let mut root = IndexMap::new();
        root.insert("a".to_string(), UnityValue::Object(pptr_internal));
        root.insert(
            "b".to_string(),
            UnityValue::Array(vec![UnityValue::Object(pptr_external)]),
        );

        scan_pptr_in_value(&UnityValue::Object(root), &mut deps);
        deps.internal.sort_unstable();
        deps.internal.dedup();
        deps.external.sort_unstable();
        deps.external.dedup();

        assert_eq!(deps.internal, vec![123]);
        assert_eq!(deps.external, vec![(2, 999)]);
    }

    #[test]
    fn test_extract_gameobject_components_and_transform_links() {
        // GameObject: m_Component = [{component:{fileID:0,pathID:10}}, {component:{fileID:0,pathID:11}}]
        let mut pptr1 = IndexMap::new();
        pptr1.insert("fileID".to_string(), UnityValue::Integer(0));
        pptr1.insert("pathID".to_string(), UnityValue::Integer(10));
        let mut item1 = IndexMap::new();
        item1.insert("component".to_string(), UnityValue::Object(pptr1));

        let mut pptr2 = IndexMap::new();
        pptr2.insert("fileID".to_string(), UnityValue::Integer(0));
        pptr2.insert("pathID".to_string(), UnityValue::Integer(11));
        let mut item2 = IndexMap::new();
        item2.insert("component".to_string(), UnityValue::Object(pptr2));

        let mut go_props = IndexMap::new();
        go_props.insert(
            "m_Component".to_string(),
            UnityValue::Array(vec![UnityValue::Object(item1), UnityValue::Object(item2)]),
        );
        let comps = extract_gameobject_components(&go_props);
        assert_eq!(comps, vec![10, 11]);

        // Transform links: m_GameObject/m_Father/m_Children
        let mut go_pptr = IndexMap::new();
        go_pptr.insert("fileID".to_string(), UnityValue::Integer(0));
        go_pptr.insert("pathID".to_string(), UnityValue::Integer(100));

        let mut parent_pptr = IndexMap::new();
        parent_pptr.insert("fileID".to_string(), UnityValue::Integer(0));
        parent_pptr.insert("pathID".to_string(), UnityValue::Integer(200));

        let mut child_pptr = IndexMap::new();
        child_pptr.insert("fileID".to_string(), UnityValue::Integer(0));
        child_pptr.insert("pathID".to_string(), UnityValue::Integer(300));

        let mut tr_props = IndexMap::new();
        tr_props.insert("m_GameObject".to_string(), UnityValue::Object(go_pptr));
        tr_props.insert("m_Father".to_string(), UnityValue::Object(parent_pptr));
        tr_props.insert(
            "m_Children".to_string(),
            UnityValue::Array(vec![UnityValue::Object(child_pptr)]),
        );

        assert_eq!(extract_transform_gameobject(&tr_props), Some(100));
        assert_eq!(extract_transform_parent(&tr_props), Some(200));
        assert_eq!(extract_transform_children(&tr_props), vec![300]);
    }
}
