//! Unity Asset Core
//!
//! Core data structures and types for Unity asset parsing.
//! This crate provides the fundamental building blocks that are shared
//! across different Unity asset formats (YAML, binary, etc.).

pub mod constants;
pub mod document;
pub mod dynamic_access;
pub mod error;
pub mod unity_class;
pub mod unity_value;

// Re-export main types
pub use constants::*;
pub use document::{DocumentFormat, UnityDocument};
pub use dynamic_access::{DynamicAccess, DynamicValue};
pub use error::{Result, UnityAssetError};
pub use unity_class::{UnityClass, UnityClassRegistry};
pub use unity_value::UnityValue;

/// Get Unity class name from class ID
pub fn get_class_name(class_id: i32) -> Option<String> {
    GLOBAL_CLASS_ID_MAP.get_class_name(class_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_functionality() {
        // 基础功能测试
        let class = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
        assert_eq!(class.class_id, 1);
        assert_eq!(class.class_name, "GameObject");
    }
}
