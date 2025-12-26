//! Unity Asset YAML Parser
//!
//! YAML format support for Unity asset parsing, providing a robust and efficient
//! Unity YAML loader based on the mature serde_yaml library.
//!
//! This crate provides parsing of Unity YAML files while maintaining exact
//! compatibility with Unity's format.
//!
//! # Examples
//!
//! ```rust
//! use unity_asset_yaml::serde_unity_loader::SerdeUnityLoader;
//!
//! let loader = SerdeUnityLoader::new();
//! let yaml = r#"
//! GameObject:
//!   m_Name: Player
//!   m_IsActive: 1
//! "#;
//!
//! let classes = loader.load_from_str(yaml)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

// Re-export core types
pub use unity_asset_core::{
    DocumentFormat, Result, UnityAssetError, UnityClass, UnityClassRegistry, UnityValue,
    constants::*,
};

// Core modules
pub mod constants;
pub mod python_like_api;
pub mod serde_unity_loader;
pub mod unity_yaml_serializer;
pub mod yaml_document;

// Re-export main types
pub use serde_unity_loader::SerdeUnityLoader;
pub use unity_yaml_serializer::UnityYamlSerializer;
pub use yaml_document::YamlDocument;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_functionality() {
        // Test that we can create a serde loader
        let _loader = SerdeUnityLoader::new();

        // Test that we can create a YAML document
        let _doc = YamlDocument::new();
    }
}
