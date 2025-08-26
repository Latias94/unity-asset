//! Unity class system
//!
//! This module implements Unity's dynamic class system, allowing for
//! runtime creation and manipulation of Unity objects.

use crate::dynamic_access::{DynamicAccess, DynamicValue};
use crate::error::{Result, UnityAssetError};
use crate::unity_value::UnityValue;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::fmt;

/// A Unity class instance
#[derive(Debug, Clone)]
pub struct UnityClass {
    /// Class ID (numeric identifier)
    pub class_id: i32,
    /// Class name (string identifier)
    pub class_name: String,
    /// YAML anchor for this object
    pub anchor: String,
    /// Extra data after the anchor line
    pub extra_anchor_data: String,
    /// Object properties
    properties: IndexMap<String, UnityValue>,
}

impl UnityClass {
    /// Create a new Unity class instance
    pub fn new(class_id: i32, class_name: String, anchor: String) -> Self {
        Self {
            class_id,
            class_name,
            anchor,
            extra_anchor_data: String::new(),
            properties: IndexMap::new(),
        }
    }

    /// Get a property value
    pub fn get(&self, key: &str) -> Option<&UnityValue> {
        self.properties.get(key)
    }

    /// Get a mutable property value
    pub fn get_mut(&mut self, key: &str) -> Option<&mut UnityValue> {
        self.properties.get_mut(key)
    }

    /// Set a property value
    pub fn set<V: Into<UnityValue>>(&mut self, key: String, value: V) {
        self.properties.insert(key, value.into());
    }

    /// Check if a property exists
    pub fn has_property(&self, key: &str) -> bool {
        self.properties.contains_key(key)
    }

    /// Get all property names
    pub fn property_names(&self) -> impl Iterator<Item = &String> {
        self.properties.keys()
    }

    /// Get all properties
    pub fn properties(&self) -> &IndexMap<String, UnityValue> {
        &self.properties
    }

    /// Get mutable properties
    pub fn properties_mut(&mut self) -> &mut IndexMap<String, UnityValue> {
        &mut self.properties
    }

    /// Update properties from another map
    pub fn update_properties(&mut self, other: IndexMap<String, UnityValue>) {
        for (key, value) in other {
            self.properties.insert(key, value);
        }
    }

    /// Get serialized properties (excluding anchor and metadata)
    pub fn serialized_properties(&self) -> IndexMap<String, UnityValue> {
        self.properties.clone()
    }

    /// Get the object name (m_Name property if it exists)
    pub fn name(&self) -> Option<&str> {
        self.get("m_Name").and_then(|v| v.as_str())
    }
}

impl fmt::Display for UnityClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.class_name, self.class_id)
    }
}

/// Implementation of dynamic property access for UnityClass
impl DynamicAccess for UnityClass {
    fn get_dynamic(&self, key: &str) -> Option<DynamicValue> {
        self.properties.get(key).map(DynamicValue::from_unity_value)
    }

    fn set_dynamic(&mut self, key: &str, value: DynamicValue) -> Result<()> {
        self.properties
            .insert(key.to_string(), value.to_unity_value());
        Ok(())
    }

    fn has_dynamic(&self, key: &str) -> bool {
        self.properties.contains_key(key)
    }

    fn keys_dynamic(&self) -> Vec<String> {
        self.properties.keys().cloned().collect()
    }
}

/// Registry for Unity class types
#[derive(Debug, Default)]
pub struct UnityClassRegistry {
    /// Map from "class_id-class_name" to class constructor
    classes: HashMap<String, fn(i32, String, String) -> UnityClass>,
}

impl UnityClassRegistry {
    /// Create a new registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a class type
    pub fn register_class<F>(&mut self, class_id: i32, class_name: &str, _constructor: F)
    where
        F: Fn(i32, String, String) -> UnityClass + 'static,
    {
        let key = format!("{}-{}", class_id, class_name);
        // For now, we'll use a simple constructor that ignores the custom function
        self.classes.insert(key, UnityClass::new);
    }

    /// Get or create a class instance
    pub fn get_or_create_class(
        &self,
        class_id: i32,
        class_name: &str,
        anchor: String,
    ) -> UnityClass {
        let key = format!("{}-{}", class_id, class_name);

        if let Some(constructor) = self.classes.get(&key) {
            constructor(class_id, class_name.to_string(), anchor)
        } else {
            // Default constructor
            UnityClass::new(class_id, class_name.to_string(), anchor)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unity_class_creation() {
        let mut class = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
        class.set("m_Name".to_string(), "TestObject");

        assert_eq!(class.class_name, "GameObject");
        assert_eq!(class.name(), Some("TestObject"));
    }

    #[test]
    fn test_unity_class_registry() {
        let registry = UnityClassRegistry::new();
        let class = registry.get_or_create_class(1, "GameObject", "123".to_string());

        assert_eq!(class.class_id, 1);
        assert_eq!(class.class_name, "GameObject");
        assert_eq!(class.anchor, "123");
    }

    #[test]
    fn test_dynamic_access() {
        let mut class = UnityClass::new(1, "GameObject".to_string(), "123".to_string());

        // Test setting and getting dynamic values
        let value = DynamicValue::String("TestName".to_string());
        class.set_dynamic("m_Name", value).unwrap();

        let retrieved = class.get_dynamic("m_Name").unwrap();
        assert_eq!(retrieved.as_string(), Some("TestName"));

        // Test has_dynamic
        assert!(class.has_dynamic("m_Name"));
        assert!(!class.has_dynamic("nonexistent"));

        // Test keys_dynamic
        let keys = class.keys_dynamic();
        assert!(keys.contains(&"m_Name".to_string()));
    }
}
