//! Dynamic property access for Unity classes
//!
//! This module provides Python-like dynamic property access for Unity objects,
//! similar to the reference library's behavior.

use crate::{Result, UnityAssetError, UnityValue};
use std::collections::HashMap;

/// Trait for dynamic property access
pub trait DynamicAccess {
    /// Get a property value with automatic type conversion
    fn get_dynamic(&self, key: &str) -> Option<DynamicValue>;

    /// Set a property value with automatic type conversion
    fn set_dynamic(&mut self, key: &str, value: DynamicValue) -> Result<()>;

    /// Check if a property exists
    fn has_dynamic(&self, key: &str) -> bool;

    /// Get all property names
    fn keys_dynamic(&self) -> Vec<String>;
}

/// Dynamic value wrapper that supports Python-like operations
#[derive(Debug, Clone, PartialEq)]
pub enum DynamicValue {
    /// String value
    String(String),
    /// Integer value
    Integer(i64),
    /// Float value
    Float(f64),
    /// Boolean value
    Bool(bool),
    /// Array value
    Array(Vec<DynamicValue>),
    /// Object value
    Object(HashMap<String, DynamicValue>),
    /// Null value
    Null,
}

impl DynamicValue {
    /// Convert from UnityValue
    pub fn from_unity_value(value: &UnityValue) -> Self {
        match value {
            UnityValue::String(s) => DynamicValue::String(s.clone()),
            UnityValue::Integer(i) => DynamicValue::Integer(*i),
            UnityValue::Float(f) => DynamicValue::Float(*f),
            UnityValue::Bool(b) => DynamicValue::Bool(*b),
            UnityValue::Array(arr) => {
                let converted: Vec<DynamicValue> =
                    arr.iter().map(DynamicValue::from_unity_value).collect();
                DynamicValue::Array(converted)
            }
            UnityValue::Object(obj) => {
                let converted: HashMap<String, DynamicValue> = obj
                    .iter()
                    .map(|(k, v)| (k.clone(), DynamicValue::from_unity_value(v)))
                    .collect();
                DynamicValue::Object(converted)
            }
            UnityValue::Null => DynamicValue::Null,
        }
    }

    /// Convert to UnityValue
    pub fn to_unity_value(&self) -> UnityValue {
        match self {
            DynamicValue::String(s) => UnityValue::String(s.clone()),
            DynamicValue::Integer(i) => UnityValue::Integer(*i),
            DynamicValue::Float(f) => UnityValue::Float(*f),
            DynamicValue::Bool(b) => UnityValue::Bool(*b),
            DynamicValue::Array(arr) => {
                let converted: Vec<UnityValue> =
                    arr.iter().map(DynamicValue::to_unity_value).collect();
                UnityValue::Array(converted)
            }
            DynamicValue::Object(obj) => {
                let converted: indexmap::IndexMap<String, UnityValue> = obj
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_unity_value()))
                    .collect();
                UnityValue::Object(converted)
            }
            DynamicValue::Null => UnityValue::Null,
        }
    }

    /// Get as string
    pub fn as_string(&self) -> Option<&str> {
        match self {
            DynamicValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// Get as integer
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            DynamicValue::Integer(i) => Some(*i),
            DynamicValue::Float(f) => Some(*f as i64),
            DynamicValue::Bool(b) => Some(if *b { 1 } else { 0 }),
            _ => None,
        }
    }

    /// Get as float
    pub fn as_float(&self) -> Option<f64> {
        match self {
            DynamicValue::Float(f) => Some(*f),
            DynamicValue::Integer(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// Get as boolean
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            DynamicValue::Bool(b) => Some(*b),
            DynamicValue::Integer(i) => Some(*i != 0),
            _ => None,
        }
    }

    /// Get as array
    pub fn as_array(&self) -> Option<&Vec<DynamicValue>> {
        match self {
            DynamicValue::Array(arr) => Some(arr),
            _ => None,
        }
    }

    /// Get as mutable array
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<DynamicValue>> {
        match self {
            DynamicValue::Array(arr) => Some(arr),
            _ => None,
        }
    }

    /// Get as object
    pub fn as_object(&self) -> Option<&HashMap<String, DynamicValue>> {
        match self {
            DynamicValue::Object(obj) => Some(obj),
            _ => None,
        }
    }

    /// Get as mutable object
    pub fn as_object_mut(&mut self) -> Option<&mut HashMap<String, DynamicValue>> {
        match self {
            DynamicValue::Object(obj) => Some(obj),
            _ => None,
        }
    }

    /// Check if value is null
    pub fn is_null(&self) -> bool {
        matches!(self, DynamicValue::Null)
    }

    /// Array indexing (similar to Python list access)
    pub fn get_index(&self, index: usize) -> Option<&DynamicValue> {
        match self {
            DynamicValue::Array(arr) => arr.get(index),
            _ => None,
        }
    }

    /// Mutable array indexing
    pub fn get_index_mut(&mut self, index: usize) -> Option<&mut DynamicValue> {
        match self {
            DynamicValue::Array(arr) => arr.get_mut(index),
            _ => None,
        }
    }

    /// Object property access (similar to Python dict access)
    pub fn get_property(&self, key: &str) -> Option<&DynamicValue> {
        match self {
            DynamicValue::Object(obj) => obj.get(key),
            _ => None,
        }
    }

    /// Mutable object property access
    pub fn get_property_mut(&mut self, key: &str) -> Option<&mut DynamicValue> {
        match self {
            DynamicValue::Object(obj) => obj.get_mut(key),
            _ => None,
        }
    }

    /// Set object property
    pub fn set_property(&mut self, key: String, value: DynamicValue) -> Result<()> {
        match self {
            DynamicValue::Object(obj) => {
                obj.insert(key, value);
                Ok(())
            }
            _ => Err(UnityAssetError::format(
                "Cannot set property on non-object value",
            )),
        }
    }

    /// Add to array
    pub fn push(&mut self, value: DynamicValue) -> Result<()> {
        match self {
            DynamicValue::Array(arr) => {
                arr.push(value);
                Ok(())
            }
            _ => Err(UnityAssetError::format("Cannot push to non-array value")),
        }
    }

    /// String concatenation (Python-like += for strings)
    pub fn concat_string(&mut self, other: &str) -> Result<()> {
        match self {
            DynamicValue::String(s) => {
                s.push_str(other);
                Ok(())
            }
            _ => Err(UnityAssetError::format(
                "Cannot concatenate to non-string value",
            )),
        }
    }

    /// Numeric addition (Python-like += for numbers)
    pub fn add_numeric(&mut self, other: f64) -> Result<()> {
        match self {
            DynamicValue::Integer(i) => {
                *i += other as i64;
                Ok(())
            }
            DynamicValue::Float(f) => {
                *f += other;
                Ok(())
            }
            _ => Err(UnityAssetError::format("Cannot add to non-numeric value")),
        }
    }
}

impl std::fmt::Display for DynamicValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DynamicValue::String(s) => write!(f, "\"{}\"", s),
            DynamicValue::Integer(i) => write!(f, "{}", i),
            DynamicValue::Float(fl) => write!(f, "{}", fl),
            DynamicValue::Bool(b) => write!(f, "{}", b),
            DynamicValue::Array(arr) => {
                write!(f, "[")?;
                for (i, item) in arr.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
            DynamicValue::Object(obj) => {
                write!(f, "{{")?;
                for (i, (key, value)) in obj.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "\"{}\": {}", key, value)?;
                }
                write!(f, "}}")
            }
            DynamicValue::Null => write!(f, "null"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dynamic_value_conversion() {
        let unity_val = UnityValue::String("test".to_string());
        let dynamic_val = DynamicValue::from_unity_value(&unity_val);

        assert_eq!(dynamic_val, DynamicValue::String("test".to_string()));
        assert_eq!(dynamic_val.to_unity_value(), unity_val);
    }

    #[test]
    fn test_dynamic_value_access() {
        let mut val = DynamicValue::String("hello".to_string());
        val.concat_string(" world").unwrap();

        assert_eq!(val.as_string(), Some("hello world"));
    }

    #[test]
    fn test_numeric_operations() {
        let mut val = DynamicValue::Integer(10);
        val.add_numeric(5.0).unwrap();

        assert_eq!(val.as_integer(), Some(15));
    }
}
