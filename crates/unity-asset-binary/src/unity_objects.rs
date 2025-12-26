//! Unity Core Object Types
//!
//! This module implements specific Unity object types like GameObject, Transform, etc.
//! These are the concrete implementations that parse TypeTree data into structured objects.

use crate::error::Result;
use indexmap::IndexMap;
use unity_asset_core::UnityValue;

/// Reference to another Unity object
#[derive(Debug, Clone)]
pub struct ObjectRef {
    pub file_id: i32,
    pub path_id: i64,
}

impl ObjectRef {
    pub fn new(file_id: i32, path_id: i64) -> Self {
        Self { file_id, path_id }
    }

    pub fn is_null(&self) -> bool {
        self.path_id == 0
    }
}

/// 3D Vector
#[derive(Debug, Clone, Default)]
pub struct Vector3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vector3 {
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// Quaternion for rotations
#[derive(Debug, Clone, Default)]
pub struct Quaternion {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Quaternion {
    pub fn new(x: f32, y: f32, z: f32, w: f32) -> Self {
        Self { x, y, z, w }
    }

    pub fn identity() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 1.0,
        }
    }
}

/// Unity GameObject
#[derive(Debug, Clone)]
pub struct GameObject {
    pub name: String,
    pub components: Vec<ObjectRef>,
    pub layer: i32,
    pub tag: String,
    pub active: bool,
}

impl GameObject {
    pub fn new() -> Self {
        Self {
            name: String::new(),
            components: Vec::new(),
            layer: 0,
            tag: "Untagged".to_string(),
            active: true,
        }
    }

    /// Parse GameObject from TypeTree data
    pub fn from_typetree(properties: &IndexMap<String, UnityValue>) -> Result<Self> {
        let mut game_object = Self::new();

        // Extract name
        if let Some(UnityValue::String(name)) = properties.get("m_Name") {
            game_object.name = name.clone();
        }

        // Extract layer
        if let Some(UnityValue::Integer(layer)) = properties.get("m_Layer") {
            game_object.layer = *layer as i32;
        }

        // Extract tag
        if let Some(UnityValue::String(tag)) = properties.get("m_Tag") {
            game_object.tag = tag.clone();
        }

        // Extract active state
        if let Some(UnityValue::Bool(active)) = properties.get("m_IsActive") {
            game_object.active = *active;
        }

        // Extract components array
        if let Some(UnityValue::Array(components_array)) = properties.get("m_Component") {
            for component in components_array {
                if let UnityValue::Object(component_obj) = component {
                    // Each component is typically a structure with file_id and path_id
                    let file_id = component_obj
                        .get("fileID")
                        .and_then(|v| match v {
                            UnityValue::Integer(id) => Some(*id as i32),
                            _ => None,
                        })
                        .unwrap_or(0);

                    let path_id = component_obj
                        .get("pathID")
                        .and_then(|v| match v {
                            UnityValue::Integer(id) => Some(*id),
                            _ => None,
                        })
                        .unwrap_or(0);

                    game_object
                        .components
                        .push(ObjectRef::new(file_id, path_id));
                }
            }
        }

        Ok(game_object)
    }
}

impl Default for GameObject {
    fn default() -> Self {
        Self::new()
    }
}

/// Unity Transform component
#[derive(Debug, Clone)]
pub struct Transform {
    pub position: Vector3,
    pub rotation: Quaternion,
    pub scale: Vector3,
    pub parent: Option<ObjectRef>,
    pub children: Vec<ObjectRef>,
}

impl Transform {
    pub fn new() -> Self {
        Self {
            position: Vector3::default(),
            rotation: Quaternion::identity(),
            scale: Vector3::new(1.0, 1.0, 1.0),
            parent: None,
            children: Vec::new(),
        }
    }

    /// Parse Transform from TypeTree data
    pub fn from_typetree(properties: &IndexMap<String, UnityValue>) -> Result<Self> {
        let mut transform = Self::new();

        // Extract position
        if let Some(position_value) = properties.get("m_LocalPosition") {
            transform.position = Self::parse_vector3(position_value)?;
        }

        // Extract rotation
        if let Some(rotation_value) = properties.get("m_LocalRotation") {
            transform.rotation = Self::parse_quaternion(rotation_value)?;
        }

        // Extract scale
        if let Some(scale_value) = properties.get("m_LocalScale") {
            transform.scale = Self::parse_vector3(scale_value)?;
        }

        // Extract parent
        if let Some(parent_value) = properties.get("m_Father") {
            transform.parent = Self::parse_object_ref(parent_value);
        }

        // Extract children
        if let Some(UnityValue::Array(children_array)) = properties.get("m_Children") {
            for child in children_array {
                if let Some(child_ref) = Self::parse_object_ref(child) {
                    transform.children.push(child_ref);
                }
            }
        }

        Ok(transform)
    }

    fn parse_vector3(value: &UnityValue) -> Result<Vector3> {
        match value {
            UnityValue::Object(obj) => {
                let x = obj
                    .get("x")
                    .and_then(|v| match v {
                        UnityValue::Float(f) => Some(*f as f32),
                        UnityValue::Integer(i) => Some(*i as f32),
                        _ => None,
                    })
                    .unwrap_or(0.0);

                let y = obj
                    .get("y")
                    .and_then(|v| match v {
                        UnityValue::Float(f) => Some(*f as f32),
                        UnityValue::Integer(i) => Some(*i as f32),
                        _ => None,
                    })
                    .unwrap_or(0.0);

                let z = obj
                    .get("z")
                    .and_then(|v| match v {
                        UnityValue::Float(f) => Some(*f as f32),
                        UnityValue::Integer(i) => Some(*i as f32),
                        _ => None,
                    })
                    .unwrap_or(0.0);

                Ok(Vector3::new(x, y, z))
            }
            _ => Ok(Vector3::default()),
        }
    }

    fn parse_quaternion(value: &UnityValue) -> Result<Quaternion> {
        match value {
            UnityValue::Object(obj) => {
                let x = obj
                    .get("x")
                    .and_then(|v| match v {
                        UnityValue::Float(f) => Some(*f as f32),
                        UnityValue::Integer(i) => Some(*i as f32),
                        _ => None,
                    })
                    .unwrap_or(0.0);

                let y = obj
                    .get("y")
                    .and_then(|v| match v {
                        UnityValue::Float(f) => Some(*f as f32),
                        UnityValue::Integer(i) => Some(*i as f32),
                        _ => None,
                    })
                    .unwrap_or(0.0);

                let z = obj
                    .get("z")
                    .and_then(|v| match v {
                        UnityValue::Float(f) => Some(*f as f32),
                        UnityValue::Integer(i) => Some(*i as f32),
                        _ => None,
                    })
                    .unwrap_or(0.0);

                let w = obj
                    .get("w")
                    .and_then(|v| match v {
                        UnityValue::Float(f) => Some(*f as f32),
                        UnityValue::Integer(i) => Some(*i as f32),
                        _ => None,
                    })
                    .unwrap_or(1.0);

                Ok(Quaternion::new(x, y, z, w))
            }
            _ => Ok(Quaternion::identity()),
        }
    }

    fn parse_object_ref(value: &UnityValue) -> Option<ObjectRef> {
        match value {
            UnityValue::Object(obj) => {
                let file_id = obj
                    .get("fileID")
                    .and_then(|v| match v {
                        UnityValue::Integer(id) => Some(*id as i32),
                        _ => None,
                    })
                    .unwrap_or(0);

                let path_id = obj
                    .get("pathID")
                    .and_then(|v| match v {
                        UnityValue::Integer(id) => Some(*id),
                        _ => None,
                    })
                    .unwrap_or(0);

                let obj_ref = ObjectRef::new(file_id, path_id);
                if obj_ref.is_null() {
                    None
                } else {
                    Some(obj_ref)
                }
            }
            _ => None,
        }
    }
}

impl Default for Transform {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gameobject_creation() {
        let game_object = GameObject::new();
        assert_eq!(game_object.name, "");
        assert_eq!(game_object.layer, 0);
        assert_eq!(game_object.tag, "Untagged");
        assert!(game_object.active);
        assert!(game_object.components.is_empty());
    }

    #[test]
    fn test_transform_creation() {
        let transform = Transform::new();
        assert_eq!(transform.position.x, 0.0);
        assert_eq!(transform.position.y, 0.0);
        assert_eq!(transform.position.z, 0.0);
        assert_eq!(transform.rotation.w, 1.0);
        assert_eq!(transform.scale.x, 1.0);
        assert_eq!(transform.scale.y, 1.0);
        assert_eq!(transform.scale.z, 1.0);
        assert!(transform.parent.is_none());
        assert!(transform.children.is_empty());
    }

    #[test]
    fn test_object_ref() {
        let obj_ref = ObjectRef::new(0, 12345);
        assert_eq!(obj_ref.file_id, 0);
        assert_eq!(obj_ref.path_id, 12345);
        assert!(!obj_ref.is_null());

        let null_ref = ObjectRef::new(0, 0);
        assert!(null_ref.is_null());
    }
}
