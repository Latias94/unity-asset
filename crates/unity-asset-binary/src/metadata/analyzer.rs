//! GameObject and component relationship analysis
//!
//! This module extracts hierarchy relationships from serialized GameObject and
//! Transform data. Cross-object references are handled by the reference graph
//! API instead.

use super::types::*;
use crate::asset::SerializedFile;
use crate::error::Result;
use crate::object::ObjectHandle;
use crate::reader::BinaryReader;
use crate::typetree::{TypeTreeParseOptions, TypeTreeSchema};
use std::collections::{HashMap, HashSet};
use unity_asset_core::{AssetLoadBudget, UnityValue};

fn parse_object_with_typetree(
    asset: &SerializedFile,
    info: &crate::asset::ObjectInfo,
    schema: &TypeTreeSchema,
    budget: &mut AssetLoadBudget,
) -> Result<indexmap::IndexMap<String, UnityValue>> {
    let bytes = ObjectHandle::new(asset, info).raw_data()?;
    let mut reader = BinaryReader::new(bytes, asset.header.byte_order());
    Ok(schema
        .read_object(&mut reader, budget, TypeTreeParseOptions::default())?
        .properties)
}

fn parse_object_best_effort(
    asset: &SerializedFile,
    info: &crate::asset::ObjectInfo,
    budget: &mut AssetLoadBudget,
) -> Result<Option<indexmap::IndexMap<String, UnityValue>>> {
    let schema = match ObjectHandle::new(asset, info).schema(budget) {
        Ok(Some(schema)) => schema,
        Ok(None) => return Ok(None),
        Err(error) if error.is_resource_error() => return Err(error),
        Err(_) => return Ok(None),
    };

    match parse_object_with_typetree(asset, info, &schema, budget) {
        Ok(values) => Ok(Some(values)),
        Err(error) if error.is_resource_error() => Err(error),
        Err(_) => Ok(None),
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
        let UnityValue::Object(obj) = item else {
            continue;
        };

        // Unity typetree usually stores { "component": {fileID, pathID} }.
        if let Some(UnityValue::Object(component_obj)) = obj.get("component") {
            if let Some((file_id, path_id)) = try_read_pptr(component_obj)
                && file_id == 0
                && path_id != 0
            {
                out.push(path_id);
            }
            continue;
        }

        // Fallback: treat the object itself as PPtr if it matches.
        if let Some((file_id, path_id)) = try_read_pptr(obj)
            && file_id == 0
            && path_id != 0
        {
            out.push(path_id);
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
        if let Some(path_id) = extract_internal_path_id(item)
            && path_id != 0
        {
            out.push(path_id);
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

/// Stateless relationship analyzer for Unity assets.
#[derive(Debug, Clone, Copy, Default)]
pub struct RelationshipAnalyzer;

impl RelationshipAnalyzer {
    /// Create a new relationship analyzer
    pub const fn new() -> Self {
        Self
    }

    /// Analyze relationships for a set of objects within a specific asset.
    ///
    /// This method parses GameObject/Transform data via TypeTree (when available) to build:
    /// - GameObject hierarchy (parent/children/depth)
    /// - Component relationships (GameObject -> Component)
    pub fn analyze_relationships_in_asset(
        &self,
        asset: &SerializedFile,
        objects: &[&crate::asset::ObjectInfo],
        budget: &mut AssetLoadBudget,
    ) -> Result<AssetRelationships> {
        let mut by_path_id: HashMap<i64, &crate::asset::ObjectInfo> = HashMap::new();
        for obj in objects {
            by_path_id.insert(obj.path_id(), *obj);
        }

        let mut gameobject_props: HashMap<i64, indexmap::IndexMap<String, UnityValue>> =
            HashMap::new();
        let mut transform_props: HashMap<i64, indexmap::IndexMap<String, UnityValue>> =
            HashMap::new();

        for obj in objects {
            match obj.class_id() {
                class_ids::GAME_OBJECT => {
                    if let Some(values) = parse_object_best_effort(asset, obj, budget)? {
                        gameobject_props.insert(obj.path_id(), values);
                    }
                }
                class_ids::TRANSFORM => {
                    if let Some(values) = parse_object_best_effort(asset, obj, budget)? {
                        transform_props.insert(obj.path_id(), values);
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
                    if let Some(info) = by_path_id.get(&component_id)
                        && info.class_id() == class_ids::TRANSFORM
                    {
                        go_transform.insert(*go_id, component_id);
                        break;
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
            if transform_id != 0
                && let Some(children) = transform_children.get(&transform_id)
            {
                for child_tr in children {
                    if let Some(child_go) = transform_to_go.get(child_tr) {
                        children_ids.push(*child_go);
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
                    .map(|info| self.get_component_type_name(info.class_id()))
                    .unwrap_or_else(|| format!("Component_{}", comp_id));

                component_relationships.push(ComponentRelationship {
                    component_id: *comp_id,
                    component_type,
                    gameobject_id: *go_id,
                });
            }
        }

        Ok(AssetRelationships {
            gameobject_hierarchy: hierarchies.into_values().collect(),
            component_relationships,
        })
    }

    /// Get component type name from type ID
    fn get_component_type_name(&self, class_id: i32) -> String {
        match class_id {
            class_ids::TRANSFORM => "Transform".to_string(),
            class_ids::MONO_BEHAVIOUR => "MonoBehaviour".to_string(),
            _ => format!("Component_{}", class_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{ObjectInfo, SerializedFileParser};
    use crate::typetree::{InMemoryTypeTreeRegistry, TypeTree, TypeTreeNode};
    use indexmap::IndexMap;
    use std::sync::Arc;

    const V22_FIXTURE: &[u8] = include_bytes!(
        "../../../unity-asset-write/tests/fixtures/serialized_file_wire/v22.assets.bin"
    );

    fn node(type_name: &str, name: &str) -> TypeTreeNode {
        TypeTreeNode::with_info(type_name.to_owned(), name.to_owned(), -1)
    }

    fn tree_with_root(root: TypeTreeNode) -> TypeTree {
        let mut tree = TypeTree::new();
        tree.add_node(root);
        tree
    }

    #[test]
    fn stripped_file_relationship_analysis_uses_external_registry() {
        let mut root = node("GameObject", "GameObject");
        root.children.push(node("int", "m_Value"));
        let mut registry = InMemoryTypeTreeRegistry::default();
        registry.insert_any(1, tree_with_root(root));

        let mut asset = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        asset.set_type_tree_enabled(false);
        asset.set_type_tree_registry(Some(Arc::new(registry)));

        let mut game_object = ObjectInfo::for_standalone_class(303, 0, 4, 1).unwrap();
        game_object.set_data(match asset.header.byte_order() {
            crate::reader::ByteOrder::Little => 1_i32.to_le_bytes().to_vec(),
            crate::reader::ByteOrder::Big => 1_i32.to_be_bytes().to_vec(),
        });
        assert!(
            parse_object_best_effort(&asset, &game_object, &mut AssetLoadBudget::default())
                .unwrap()
                .is_some()
        );
        let analyzer = RelationshipAnalyzer::new();
        let relationships = analyzer
            .analyze_relationships_in_asset(
                &asset,
                &[&game_object],
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(relationships.gameobject_hierarchy.len(), 1);
        assert_eq!(relationships.gameobject_hierarchy[0].gameobject_id, 303);
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
