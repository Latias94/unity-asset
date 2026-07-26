//! Unity Asset YAML Parser
//!
//! YAML format support for Unity assets. The crate provides a caller-budgeted,
//! non-recursive event parser for owned source images together with compatibility
//! loading, serialization, and reference-scanning APIs.
//!
//! # Budgeted parsing
//!
//! ```rust
//! use std::sync::Arc;
//!
//! use unity_asset_yaml::{AssetLoadBudget, UnityDocument, parse_budgeted_yaml_source};
//!
//! let encoded: Arc<[u8]> = Arc::from(
//!     b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &42\nGameObject:\n  m_Name: Player\n"
//!         .as_slice(),
//! );
//! let mut budget = AssetLoadBudget::default();
//! let source = parse_budgeted_yaml_source(encoded, &mut budget)?;
//! assert_eq!(source.document().entries().len(), 1);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

// Re-export core types
pub use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, BudgetedSourceBytes, DocumentFormat, Result,
    UnityAssetError, UnityClass, UnityClassRegistry, UnityDocument, UnityValue, constants::*,
};

// Core modules
mod budgeted;
pub mod constants;
pub mod python_like_api;
pub mod reference;
pub mod serde_unity_loader;
pub mod unity_yaml_serializer;
pub mod yaml_document;

// Re-export main types
pub use budgeted::{
    BudgetedYamlError, BudgetedYamlSource, parse_budgeted_yaml_source,
    parse_prebudgeted_yaml_source,
};
pub use reference::{
    YamlReferenceDiagnostic, YamlReferenceField, YamlReferenceOccurrence, YamlReferenceRawTarget,
    YamlReferenceRawTargetRef, YamlReferenceScan, YamlReferenceScanError, YamlReferenceScanStats,
    YamlReferenceShape, YamlReferenceTarget, YamlValueKind, scan_reference_class_occurrences,
    scan_reference_occurrences,
};
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

    #[test]
    fn budgeted_parser_is_available_from_the_crate_root() {
        let encoded: std::sync::Arc<[u8]> = std::sync::Arc::from(b"root: value\n".as_slice());
        let mut budget = AssetLoadBudget::default();
        let parsed: std::result::Result<BudgetedYamlSource, BudgetedYamlError> =
            parse_budgeted_yaml_source(encoded, &mut budget);

        assert_eq!(parsed.unwrap().document().entries().len(), 1);
    }
}
