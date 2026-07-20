//! Unity class system
//!
//! This module implements Unity's dynamic class system, allowing for
//! runtime creation and manipulation of Unity objects.

use crate::budget::AssetLoadBudget;
use crate::dynamic_access::{DynamicAccess, DynamicValue};
use crate::error::Result as UnityResult;
use crate::field_path::{FieldPath, FieldPathSegment};
use crate::unity_value::{
    UnityValue, UnityValueCloneError, UnityValueKind, ValuePathError, clone_string,
    try_clone_object_with_budget,
};
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
    /// Deeply clones this class while charging all owned string and property
    /// storage to `budget` before allocation.
    ///
    /// The property map is the depth-zero root and shares the caller's member
    /// and depth ledgers with every nested value.
    pub fn try_clone_with_budget(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, UnityValueCloneError> {
        let class_name = clone_string(&self.class_name, budget, "Unity class name")?;
        let anchor = clone_string(&self.anchor, budget, "Unity class anchor")?;
        let extra_anchor_data = clone_string(
            &self.extra_anchor_data,
            budget,
            "Unity class extra anchor data",
        )?;
        let properties = try_clone_object_with_budget(&self.properties, budget)?;
        Ok(Self {
            class_id: self.class_id,
            class_name,
            anchor,
            extra_anchor_data,
            properties,
        })
    }

    /// Create a new Unity class instance
    pub fn new(class_id: i32, class_name: String, anchor: String) -> Self {
        Self::with_properties(class_id, class_name, anchor, IndexMap::new())
    }

    /// Creates a class by taking ownership of an already materialized property map.
    ///
    /// Parsers should prefer this constructor so a budgeted map is not reallocated field by field.
    pub fn with_properties(
        class_id: i32,
        class_name: String,
        anchor: String,
        properties: IndexMap<String, UnityValue>,
    ) -> Self {
        Self {
            class_id,
            class_name,
            anchor,
            extra_anchor_data: String::new(),
            properties,
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

    /// Resolves a property path without cloning its field names or values.
    ///
    /// A `UnityClass` stores a property map rather than a root [`UnityValue`], so
    /// [`FieldPath::root`] returns [`ValuePathError::ClassRoot`]. Use
    /// [`Self::properties`] when the property root itself is required.
    pub fn value_at_path(&self, path: &FieldPath) -> Result<&UnityValue, ValuePathError> {
        let (first, remaining) = path
            .segments()
            .split_first()
            .ok_or(ValuePathError::ClassRoot)?;
        let mut current = match first {
            FieldPathSegment::Field(name) => self
                .properties
                .get(name)
                .ok_or(ValuePathError::MissingField { segment: 0 })?,
            FieldPathSegment::Index(_) => {
                return Err(ValuePathError::ExpectedArray {
                    segment: 0,
                    actual: UnityValueKind::Object,
                });
            }
        };
        for (offset, segment) in remaining.iter().enumerate() {
            current = current.value_at_segment(segment, offset + 1)?;
        }
        Ok(current)
    }

    /// Mutably resolves a property path without cloning its field names or values.
    ///
    /// The root-path behavior matches [`Self::value_at_path`].
    pub fn value_at_path_mut(
        &mut self,
        path: &FieldPath,
    ) -> Result<&mut UnityValue, ValuePathError> {
        let (first, remaining) = path
            .segments()
            .split_first()
            .ok_or(ValuePathError::ClassRoot)?;
        let mut current = match first {
            FieldPathSegment::Field(name) => self
                .properties
                .get_mut(name)
                .ok_or(ValuePathError::MissingField { segment: 0 })?,
            FieldPathSegment::Index(_) => {
                return Err(ValuePathError::ExpectedArray {
                    segment: 0,
                    actual: UnityValueKind::Object,
                });
            }
        };
        for (offset, segment) in remaining.iter().enumerate() {
            current = current.value_at_segment_mut(segment, offset + 1)?;
        }
        Ok(current)
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

    fn set_dynamic(&mut self, key: &str, value: DynamicValue) -> UnityResult<()> {
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
    use crate::budget::AssetLoadLimits;

    #[test]
    fn test_unity_class_creation() {
        let mut class = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
        class.set("m_Name".to_string(), "TestObject");

        assert_eq!(class.class_name, "GameObject");
        assert_eq!(class.name(), Some("TestObject"));
    }

    #[test]
    fn with_properties_reuses_the_materialized_map() {
        let mut properties = IndexMap::with_capacity(4);
        properties.insert(
            "m_Name".to_string(),
            UnityValue::String("TestObject".to_string()),
        );
        let capacity = properties.capacity();

        let class =
            UnityClass::with_properties(1, "GameObject".to_string(), "123".to_string(), properties);

        assert_eq!(class.properties().capacity(), capacity);
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
    fn class_paths_resolve_properties_and_reject_the_value_root() {
        let mut nested = IndexMap::new();
        nested.insert(
            "values".to_owned(),
            UnityValue::Array(vec![UnityValue::Integer(3)]),
        );
        let mut class = UnityClass::new(1, "GameObject".to_owned(), "1".to_owned());
        class.set("nested".to_owned(), UnityValue::Object(nested));

        assert_eq!(
            class.value_at_path(&FieldPath::root()),
            Err(ValuePathError::ClassRoot)
        );
        let path = FieldPath::root()
            .push_field("nested")
            .and_then(|path| path.push_field("values"))
            .and_then(|path| path.push_index(0))
            .expect("valid path");
        assert_eq!(class.value_at_path(&path), Ok(&UnityValue::Integer(3)));

        *class.value_at_path_mut(&path).expect("path resolves") = UnityValue::Unsigned(u64::MAX);
        assert_eq!(
            class.value_at_path(&path),
            Ok(&UnityValue::Unsigned(u64::MAX))
        );
    }

    #[test]
    fn class_path_errors_report_root_shape_and_missing_fields() {
        let mut class = UnityClass::new(1, "GameObject".to_owned(), "1".to_owned());
        assert_eq!(
            class.value_at_path_mut(&FieldPath::root()),
            Err(ValuePathError::ClassRoot)
        );
        let index = FieldPath::root().push_index(0).expect("valid path");
        assert_eq!(
            class.value_at_path(&index),
            Err(ValuePathError::ExpectedArray {
                segment: 0,
                actual: UnityValueKind::Object,
            })
        );
        let missing = FieldPath::root().push_field("missing").expect("valid path");
        assert_eq!(
            class.value_at_path(&missing),
            Err(ValuePathError::MissingField { segment: 0 })
        );
        assert_eq!(
            class.value_at_path_mut(&missing),
            Err(ValuePathError::MissingField { segment: 0 })
        );
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

    #[test]
    fn budgeted_clone_preserves_class_metadata_and_property_order() {
        let mut properties = IndexMap::new();
        properties.insert("first".to_owned(), UnityValue::String("one".to_owned()));
        properties.insert(
            "second".to_owned(),
            UnityValue::Array(vec![UnityValue::Unsigned(u64::MAX)]),
        );
        let mut source = UnityClass::with_properties(
            114,
            "MonoBehaviour".to_owned(),
            "9001".to_owned(),
            properties,
        );
        source.extra_anchor_data = " stripped".to_owned();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits::default()).unwrap();

        let cloned = source.try_clone_with_budget(&mut budget).unwrap();

        assert_eq!(cloned.class_id, source.class_id);
        assert_eq!(cloned.class_name, source.class_name);
        assert_eq!(cloned.anchor, source.anchor);
        assert_eq!(cloned.extra_anchor_data, source.extra_anchor_data);
        assert_eq!(cloned.properties(), source.properties());
        assert!(cloned.properties().keys().eq(source.properties().keys()));
        assert_eq!(budget.usage().members, 3);
        assert_eq!(budget.usage().max_observed_depth, 2);
    }
}
