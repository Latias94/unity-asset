//! Unity class system
//!
//! This module defines the immutable semantic representation of a Unity object.

use crate::budget::AssetLoadBudget;
use crate::field_path::{FieldPath, FieldPathSegment};
use crate::unity_value::{
    UnityValue, UnityValueCloneError, UnityValueKind, ValuePathError, clone_string,
    try_clone_object_with_budget,
};
use indexmap::IndexMap;
use std::fmt;

/// Immutable identity and wire metadata for a [`UnityClass`].
///
/// Callers may inspect this header or carry it through an owned
/// [`UnityClass::into_parts`] / [`UnityClass::from_parts`] rebuild, but cannot
/// alter the identity of a live class in place.
#[derive(Debug, Clone)]
pub struct UnityClassHeader {
    class_id: i32,
    class_name: String,
    anchor: String,
    extra_anchor_data: String,
}

impl UnityClassHeader {
    /// Creates complete class metadata from parser-owned strings.
    pub fn new(
        class_id: i32,
        class_name: String,
        anchor: String,
        extra_anchor_data: String,
    ) -> Self {
        Self {
            class_id,
            class_name,
            anchor,
            extra_anchor_data,
        }
    }

    /// Numeric Unity class identifier.
    pub const fn class_id(&self) -> i32 {
        self.class_id
    }

    /// Canonical Unity class name.
    pub fn class_name(&self) -> &str {
        &self.class_name
    }

    /// YAML anchor or binary path identifier text.
    pub fn anchor(&self) -> &str {
        &self.anchor
    }

    /// Unparsed data following a Unity YAML anchor declaration.
    pub fn extra_anchor_data(&self) -> &str {
        &self.extra_anchor_data
    }
}

/// An immutable Unity class instance.
#[derive(Debug, Clone)]
pub struct UnityClass {
    header: UnityClassHeader,
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
        let class_name = clone_string(self.class_name(), budget, "Unity class name")?;
        let anchor = clone_string(self.anchor(), budget, "Unity class anchor")?;
        let extra_anchor_data = clone_string(
            self.extra_anchor_data(),
            budget,
            "Unity class extra anchor data",
        )?;
        let properties = try_clone_object_with_budget(&self.properties, budget)?;
        Ok(Self::from_parts(
            UnityClassHeader::new(self.class_id(), class_name, anchor, extra_anchor_data),
            properties,
        ))
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
        Self::from_parts(
            UnityClassHeader::new(class_id, class_name, anchor, String::new()),
            properties,
        )
    }

    /// Rebuilds a class from immutable metadata and an owned property map.
    pub fn from_parts(header: UnityClassHeader, properties: IndexMap<String, UnityValue>) -> Self {
        Self { header, properties }
    }

    /// Consumes the class into immutable metadata and its owned property map.
    pub fn into_parts(self) -> (UnityClassHeader, IndexMap<String, UnityValue>) {
        (self.header, self.properties)
    }

    /// Numeric Unity class identifier.
    pub const fn class_id(&self) -> i32 {
        self.header.class_id()
    }

    /// Canonical Unity class name.
    pub fn class_name(&self) -> &str {
        self.header.class_name()
    }

    /// YAML anchor or binary path identifier text.
    pub fn anchor(&self) -> &str {
        self.header.anchor()
    }

    /// Unparsed data following a Unity YAML anchor declaration.
    pub fn extra_anchor_data(&self) -> &str {
        self.header.extra_anchor_data()
    }

    /// Get a property value
    pub fn get(&self, key: &str) -> Option<&UnityValue> {
        self.properties.get(key)
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
        write!(f, "{}({})", self.class_name(), self.class_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::AssetLoadLimits;

    #[test]
    fn test_unity_class_creation() {
        let class = UnityClass::with_properties(
            1,
            "GameObject".to_string(),
            "123".to_string(),
            IndexMap::from([(
                "m_Name".to_string(),
                UnityValue::String("TestObject".to_string()),
            )]),
        );

        assert_eq!(class.class_name(), "GameObject");
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
    fn consuming_parts_rebuild_preserves_metadata_without_live_mutation() {
        let source = UnityClass::from_parts(
            UnityClassHeader::new(
                114,
                "MonoBehaviour".to_owned(),
                "9001".to_owned(),
                "stripped".to_owned(),
            ),
            IndexMap::new(),
        );

        let (header, mut properties) = source.into_parts();
        properties.insert("enabled".to_owned(), UnityValue::Bool(true));
        let rebuilt = UnityClass::from_parts(header, properties);

        assert_eq!(rebuilt.class_id(), 114);
        assert_eq!(rebuilt.class_name(), "MonoBehaviour");
        assert_eq!(rebuilt.anchor(), "9001");
        assert_eq!(rebuilt.extra_anchor_data(), "stripped");
        assert_eq!(rebuilt.get("enabled"), Some(&UnityValue::Bool(true)));
    }

    #[test]
    fn class_paths_resolve_properties_and_reject_the_value_root() {
        let mut nested = IndexMap::new();
        nested.insert(
            "values".to_owned(),
            UnityValue::Array(vec![UnityValue::Integer(3)]),
        );
        let class = UnityClass::with_properties(
            1,
            "GameObject".to_owned(),
            "1".to_owned(),
            IndexMap::from([("nested".to_owned(), UnityValue::Object(nested))]),
        );

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
    }

    #[test]
    fn class_path_errors_report_root_shape_and_missing_fields() {
        let class = UnityClass::new(1, "GameObject".to_owned(), "1".to_owned());
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
    }

    #[test]
    fn budgeted_clone_preserves_class_metadata_and_property_order() {
        let mut properties = IndexMap::new();
        properties.insert("first".to_owned(), UnityValue::String("one".to_owned()));
        properties.insert(
            "second".to_owned(),
            UnityValue::Array(vec![UnityValue::Unsigned(u64::MAX)]),
        );
        let source = UnityClass::from_parts(
            UnityClassHeader::new(
                114,
                "MonoBehaviour".to_owned(),
                "9001".to_owned(),
                " stripped".to_owned(),
            ),
            properties,
        );
        let mut budget = AssetLoadBudget::new(AssetLoadLimits::default()).unwrap();

        let cloned = source.try_clone_with_budget(&mut budget).unwrap();

        assert_eq!(cloned.class_id(), source.class_id());
        assert_eq!(cloned.class_name(), source.class_name());
        assert_eq!(cloned.anchor(), source.anchor());
        assert_eq!(cloned.extra_anchor_data(), source.extra_anchor_data());
        assert_eq!(cloned.properties(), source.properties());
        assert!(cloned.properties().keys().eq(source.properties().keys()));
        assert_eq!(budget.usage().members, 3);
        assert_eq!(budget.usage().max_observed_depth, 2);
    }
}
