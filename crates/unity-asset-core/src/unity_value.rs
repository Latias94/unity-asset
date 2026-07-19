//! Unity value types
//!
//! This module defines the UnityValue enum and related functionality
//! for representing Unity asset values in a type-safe manner.

use std::fmt;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::field_path::{FieldPath, FieldPathSegment};

/// A stable description of a [`UnityValue`] shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnityValueKind {
    Null,
    Bool,
    Integer,
    Unsigned,
    Float,
    String,
    Array,
    Bytes,
    Object,
}

impl UnityValueKind {
    /// Returns the stable lowercase name used in diagnostics and serialized data.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool => "bool",
            Self::Integer => "integer",
            Self::Unsigned => "unsigned",
            Self::Float => "float",
            Self::String => "string",
            Self::Array => "array",
            Self::Bytes => "bytes",
            Self::Object => "object",
        }
    }
}

impl fmt::Display for UnityValueKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Failure while resolving a [`FieldPath`] against a Unity value tree.
///
/// The error retains only stable scalar diagnostics. Resolution never clones a
/// path or field name and therefore does not allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ValuePathError {
    #[error("a UnityClass has no UnityValue at the root field path")]
    ClassRoot,
    #[error("field is missing at path segment {segment}")]
    MissingField { segment: usize },
    #[error("expected an object at path segment {segment}, found {actual}")]
    ExpectedObject {
        segment: usize,
        actual: UnityValueKind,
    },
    #[error("expected an array at path segment {segment}, found {actual}")]
    ExpectedArray {
        segment: usize,
        actual: UnityValueKind,
    },
    #[error("index {index} is outside array length {length} at path segment {segment}")]
    IndexOutOfBounds {
        segment: usize,
        index: u32,
        length: usize,
    },
}

impl ValuePathError {
    /// Returns the zero-based failing segment, or `None` for an empty class path.
    #[must_use]
    pub const fn segment(&self) -> Option<usize> {
        match self {
            Self::ClassRoot => None,
            Self::MissingField { segment }
            | Self::ExpectedObject { segment, .. }
            | Self::ExpectedArray { segment, .. }
            | Self::IndexOutOfBounds { segment, .. } => Some(*segment),
        }
    }
}

/// A Unity value that can be stored in a Unity class
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UnityValue {
    Null,
    Bool(bool),
    Integer(i64),
    /// An unsigned integer that cannot be represented by [`Self::Integer`].
    Unsigned(u64),
    Float(f64),
    String(String),
    Array(Vec<UnityValue>),
    #[serde(with = "serde_bytes")]
    Bytes(Vec<u8>),
    Object(IndexMap<String, UnityValue>),
}

impl UnityValue {
    /// Returns the stable shape discriminator for this value.
    #[must_use]
    pub const fn kind(&self) -> UnityValueKind {
        match self {
            Self::Null => UnityValueKind::Null,
            Self::Bool(_) => UnityValueKind::Bool,
            Self::Integer(_) => UnityValueKind::Integer,
            Self::Unsigned(_) => UnityValueKind::Unsigned,
            Self::Float(_) => UnityValueKind::Float,
            Self::String(_) => UnityValueKind::String,
            Self::Array(_) => UnityValueKind::Array,
            Self::Bytes(_) => UnityValueKind::Bytes,
            Self::Object(_) => UnityValueKind::Object,
        }
    }

    /// Resolves one field-path segment without cloning the segment or value.
    pub(crate) fn value_at_segment(
        &self,
        segment: &FieldPathSegment,
        segment_ordinal: usize,
    ) -> Result<&Self, ValuePathError> {
        let actual = self.kind();
        match segment {
            FieldPathSegment::Field(name) => {
                let Self::Object(fields) = self else {
                    return Err(ValuePathError::ExpectedObject {
                        segment: segment_ordinal,
                        actual,
                    });
                };
                fields.get(name).ok_or(ValuePathError::MissingField {
                    segment: segment_ordinal,
                })
            }
            FieldPathSegment::Index(index) => {
                let Self::Array(values) = self else {
                    return Err(ValuePathError::ExpectedArray {
                        segment: segment_ordinal,
                        actual,
                    });
                };
                let index_usize =
                    usize::try_from(*index).map_err(|_| ValuePathError::IndexOutOfBounds {
                        segment: segment_ordinal,
                        index: *index,
                        length: values.len(),
                    })?;
                values
                    .get(index_usize)
                    .ok_or(ValuePathError::IndexOutOfBounds {
                        segment: segment_ordinal,
                        index: *index,
                        length: values.len(),
                    })
            }
        }
    }

    /// Mutably resolves one field-path segment without cloning the segment or value.
    pub(crate) fn value_at_segment_mut(
        &mut self,
        segment: &FieldPathSegment,
        segment_ordinal: usize,
    ) -> Result<&mut Self, ValuePathError> {
        let actual = self.kind();
        match segment {
            FieldPathSegment::Field(name) => {
                let Self::Object(fields) = self else {
                    return Err(ValuePathError::ExpectedObject {
                        segment: segment_ordinal,
                        actual,
                    });
                };
                fields.get_mut(name).ok_or(ValuePathError::MissingField {
                    segment: segment_ordinal,
                })
            }
            FieldPathSegment::Index(index) => {
                let Self::Array(values) = self else {
                    return Err(ValuePathError::ExpectedArray {
                        segment: segment_ordinal,
                        actual,
                    });
                };
                let length = values.len();
                let index_usize =
                    usize::try_from(*index).map_err(|_| ValuePathError::IndexOutOfBounds {
                        segment: segment_ordinal,
                        index: *index,
                        length,
                    })?;
                values
                    .get_mut(index_usize)
                    .ok_or(ValuePathError::IndexOutOfBounds {
                        segment: segment_ordinal,
                        index: *index,
                        length,
                    })
            }
        }
    }

    /// Resolves borrowed path segments. An empty segment slice returns this value.
    pub fn value_at_segments(
        &self,
        segments: &[FieldPathSegment],
    ) -> Result<&Self, ValuePathError> {
        let mut current = self;
        for (segment_ordinal, segment) in segments.iter().enumerate() {
            current = current.value_at_segment(segment, segment_ordinal)?;
        }
        Ok(current)
    }

    /// Mutably resolves borrowed path segments. An empty segment slice returns this value.
    pub fn value_at_segments_mut(
        &mut self,
        segments: &[FieldPathSegment],
    ) -> Result<&mut Self, ValuePathError> {
        let mut current = self;
        for (segment_ordinal, segment) in segments.iter().enumerate() {
            current = current.value_at_segment_mut(segment, segment_ordinal)?;
        }
        Ok(current)
    }

    /// Resolves a field path. The root path returns this value.
    pub fn value_at_path(&self, path: &FieldPath) -> Result<&Self, ValuePathError> {
        self.value_at_segments(path.segments())
    }

    /// Mutably resolves a field path. The root path returns this value.
    pub fn value_at_path_mut(&mut self, path: &FieldPath) -> Result<&mut Self, ValuePathError> {
        self.value_at_segments_mut(path.segments())
    }

    /// Check if the value is null
    pub fn is_null(&self) -> bool {
        matches!(self, UnityValue::Null)
    }

    /// Get as boolean
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            UnityValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Get as integer
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            UnityValue::Integer(i) => Some(*i),
            UnityValue::Unsigned(i) => i64::try_from(*i).ok(),
            _ => None,
        }
    }

    /// Get as an unsigned integer without losing range information.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            UnityValue::Integer(i) => u64::try_from(*i).ok(),
            UnityValue::Unsigned(i) => Some(*i),
            _ => None,
        }
    }

    /// Get as float
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            UnityValue::Float(f) => Some(*f),
            UnityValue::Integer(i) => Some(*i as f64),
            UnityValue::Unsigned(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// Get as string
    pub fn as_str(&self) -> Option<&str> {
        match self {
            UnityValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// Get as array
    pub fn as_array(&self) -> Option<&Vec<UnityValue>> {
        match self {
            UnityValue::Array(arr) => Some(arr),
            _ => None,
        }
    }

    /// Get as bytes
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            UnityValue::Bytes(b) => Some(b.as_slice()),
            _ => None,
        }
    }

    /// Get as object
    pub fn as_object(&self) -> Option<&IndexMap<String, UnityValue>> {
        match self {
            UnityValue::Object(obj) => Some(obj),
            _ => None,
        }
    }

    /// Get mutable reference as object
    pub fn as_object_mut(&mut self) -> Option<&mut IndexMap<String, UnityValue>> {
        match self {
            UnityValue::Object(obj) => Some(obj),
            _ => None,
        }
    }
}

impl fmt::Display for UnityValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnityValue::Null => write!(f, "null"),
            UnityValue::Bool(b) => write!(f, "{}", b),
            UnityValue::Integer(i) => write!(f, "{}", i),
            UnityValue::Unsigned(i) => write!(f, "{}", i),
            UnityValue::Float(fl) => write!(f, "{}", fl),
            UnityValue::String(s) => write!(f, "{}", s),
            UnityValue::Array(arr) => {
                write!(f, "[")?;
                for (i, item) in arr.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
            UnityValue::Bytes(b) => write!(f, "<bytes len={}>", b.len()),
            UnityValue::Object(obj) => {
                write!(f, "{{")?;
                for (i, (key, value)) in obj.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", key, value)?;
                }
                write!(f, "}}")
            }
        }
    }
}

// Conversion implementations
impl From<bool> for UnityValue {
    fn from(b: bool) -> Self {
        UnityValue::Bool(b)
    }
}

impl From<i32> for UnityValue {
    fn from(i: i32) -> Self {
        UnityValue::Integer(i as i64)
    }
}

impl From<i64> for UnityValue {
    fn from(i: i64) -> Self {
        UnityValue::Integer(i)
    }
}

impl From<u64> for UnityValue {
    fn from(i: u64) -> Self {
        i64::try_from(i)
            .map(UnityValue::Integer)
            .unwrap_or(UnityValue::Unsigned(i))
    }
}

impl From<f32> for UnityValue {
    fn from(f: f32) -> Self {
        UnityValue::Float(f as f64)
    }
}

impl From<f64> for UnityValue {
    fn from(f: f64) -> Self {
        UnityValue::Float(f)
    }
}

impl From<String> for UnityValue {
    fn from(s: String) -> Self {
        UnityValue::String(s)
    }
}

impl From<&str> for UnityValue {
    fn from(s: &str) -> Self {
        UnityValue::String(s.to_string())
    }
}

impl From<Vec<UnityValue>> for UnityValue {
    fn from(arr: Vec<UnityValue>) -> Self {
        UnityValue::Array(arr)
    }
}

impl From<IndexMap<String, UnityValue>> for UnityValue {
    fn from(obj: IndexMap<String, UnityValue>) -> Self {
        UnityValue::Object(obj)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested_value() -> UnityValue {
        let mut child = IndexMap::new();
        child.insert(
            "items".to_owned(),
            UnityValue::Array(vec![UnityValue::Unsigned(u64::MAX)]),
        );
        let mut root = IndexMap::new();
        root.insert("child".to_owned(), UnityValue::Object(child));
        UnityValue::Object(root)
    }

    #[test]
    fn test_unity_value_creation() {
        let val = UnityValue::String("test".to_string());
        assert_eq!(val.as_str(), Some("test"));
    }

    #[test]
    fn test_unity_value_conversions() {
        // Test various value types
        let bool_val: UnityValue = true.into();
        assert_eq!(bool_val.as_bool(), Some(true));

        let int_val: UnityValue = 42i32.into();
        assert_eq!(int_val.as_i64(), Some(42));

        let float_val: UnityValue = std::f64::consts::PI.into();
        assert_eq!(float_val.as_f64(), Some(std::f64::consts::PI));

        let string_val: UnityValue = "test".into();
        assert_eq!(string_val.as_str(), Some("test"));

        // Test null
        let null_val = UnityValue::Null;
        assert!(null_val.is_null());
    }

    #[test]
    fn test_unity_value_display() {
        let val = UnityValue::String("test".to_string());
        assert_eq!(format!("{}", val), "test");

        let val = UnityValue::Integer(42);
        assert_eq!(format!("{}", val), "42");

        let val = UnityValue::Bool(true);
        assert_eq!(format!("{}", val), "true");
    }

    #[test]
    fn unsigned_values_above_i64_max_round_trip_without_sign_loss() {
        let value = UnityValue::Unsigned(u64::MAX);

        assert_eq!(value.as_u64(), Some(u64::MAX));
        assert_eq!(format!("{value}"), u64::MAX.to_string());

        let encoded = serde_json::to_string(&value).unwrap();
        assert_eq!(encoded, u64::MAX.to_string());
        assert_eq!(serde_json::from_str::<UnityValue>(&encoded).unwrap(), value);
        assert_eq!(UnityValue::from(42_u64), UnityValue::Integer(42));
    }

    #[test]
    fn value_paths_resolve_fields_indices_and_root_without_allocation() {
        let value = nested_value();
        let root = value
            .value_at_path(&FieldPath::root())
            .expect("root path resolves");
        assert!(std::ptr::eq(root, &value));

        let path = FieldPath::root()
            .push_field("child")
            .and_then(|path| path.push_field("items"))
            .and_then(|path| path.push_index(0))
            .expect("valid path");
        assert_eq!(
            value.value_at_path(&path),
            Ok(&UnityValue::Unsigned(u64::MAX))
        );
    }

    #[test]
    fn value_path_errors_preserve_segment_and_stable_actual_kind() {
        let value = nested_value();
        let missing = FieldPath::root().push_field("missing").expect("valid path");
        assert_eq!(
            value.value_at_path(&missing),
            Err(ValuePathError::MissingField { segment: 0 })
        );

        let expected_object = FieldPath::root()
            .push_field("child")
            .and_then(|path| path.push_field("items"))
            .and_then(|path| path.push_index(0))
            .and_then(|path| path.push_field("invalid"))
            .expect("valid path");
        assert_eq!(
            value.value_at_path(&expected_object),
            Err(ValuePathError::ExpectedObject {
                segment: 3,
                actual: UnityValueKind::Unsigned,
            })
        );
        assert_eq!(UnityValueKind::Unsigned.as_str(), "unsigned");
        assert_eq!(
            serde_json::to_string(&UnityValueKind::Unsigned).expect("kind serializes"),
            "\"unsigned\""
        );

        let expected_array = FieldPath::root()
            .push_field("child")
            .and_then(|path| path.push_index(0))
            .expect("valid path");
        assert_eq!(
            value.value_at_path(&expected_array),
            Err(ValuePathError::ExpectedArray {
                segment: 1,
                actual: UnityValueKind::Object,
            })
        );

        let out_of_bounds = FieldPath::root()
            .push_field("child")
            .and_then(|path| path.push_field("items"))
            .and_then(|path| path.push_index(1))
            .expect("valid path");
        assert_eq!(
            value.value_at_path(&out_of_bounds),
            Err(ValuePathError::IndexOutOfBounds {
                segment: 2,
                index: 1,
                length: 1,
            })
        );
    }

    #[test]
    fn mutable_value_path_updates_the_selected_value() {
        let mut value = nested_value();
        let path = FieldPath::root()
            .push_field("child")
            .and_then(|path| path.push_field("items"))
            .and_then(|path| path.push_index(0))
            .expect("valid path");
        *value.value_at_path_mut(&path).expect("path resolves") = UnityValue::Integer(7);
        assert_eq!(value.value_at_path(&path), Ok(&UnityValue::Integer(7)));
    }

    #[test]
    fn mutable_value_path_errors_match_immutable_resolution() {
        let paths = [
            FieldPath::root().push_field("missing").expect("valid path"),
            FieldPath::root()
                .push_field("child")
                .and_then(|path| path.push_field("items"))
                .and_then(|path| path.push_index(0))
                .and_then(|path| path.push_field("invalid"))
                .expect("valid path"),
            FieldPath::root()
                .push_field("child")
                .and_then(|path| path.push_index(0))
                .expect("valid path"),
            FieldPath::root()
                .push_field("child")
                .and_then(|path| path.push_field("items"))
                .and_then(|path| path.push_index(1))
                .expect("valid path"),
        ];

        for path in paths {
            let immutable_error = nested_value()
                .value_at_path(&path)
                .expect_err("path must fail");
            let mutable_error = nested_value()
                .value_at_path_mut(&path)
                .expect_err("path must fail");
            assert_eq!(mutable_error, immutable_error);
        }
    }
}
