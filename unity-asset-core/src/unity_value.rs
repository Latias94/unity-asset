//! Unity value types
//!
//! This module defines the UnityValue enum and related functionality
//! for representing Unity asset values in a type-safe manner.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A Unity value that can be stored in a Unity class
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UnityValue {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    Array(Vec<UnityValue>),
    Object(IndexMap<String, UnityValue>),
}

impl UnityValue {
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
            _ => None,
        }
    }

    /// Get as float
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            UnityValue::Float(f) => Some(*f),
            UnityValue::Integer(i) => Some(*i as f64),
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

        let float_val: UnityValue = 3.14f64.into();
        assert_eq!(float_val.as_f64(), Some(3.14));

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
}
