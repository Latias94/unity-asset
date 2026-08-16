//! Unity Asset YAML Parser
//!
//! YAML format support for Unity assets. The crate provides a caller-budgeted,
//! non-recursive event parser for owned source images together with serialization
//! and reference-scanning APIs.
//!
//! # Budgeted parsing
//!
//! ```rust
//! use unity_asset_core::AssetLoadBudget;
//! use unity_asset_yaml::load_budgeted_yaml_path;
//!
//! let mut budget = AssetLoadBudget::default();
//! let source = load_budgeted_yaml_path("Player.prefab", &mut budget)?;
//! assert_eq!(source.document().entries().len(), 1);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

// Core modules
mod budgeted;
pub mod reference;
pub mod unity_yaml_serializer;
pub mod yaml_document;

// Re-export main types
#[cfg(feature = "async")]
pub use budgeted::load_budgeted_yaml_path_async;
pub use budgeted::{
    BudgetedYamlError, BudgetedYamlSource, PreparedYamlProof, YamlInspection,
    load_budgeted_yaml_path, parse_budgeted_yaml_source, parse_prebudgeted_yaml_source,
};
pub use reference::{
    YamlReferenceClassification, YamlReferenceDiagnostic, YamlReferenceField,
    YamlReferenceOccurrence, YamlReferenceRawTarget, YamlReferenceRawTargetRef, YamlReferenceScan,
    YamlReferenceScanError, YamlReferenceScanStats, YamlReferenceShape, YamlReferenceTarget,
    YamlValueKind, classify_reference_value, scan_reference_class_occurrences,
    scan_reference_occurrences,
};
pub use unity_yaml_serializer::UnityYamlSerializer;
pub use yaml_document::YamlDocument;

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::AssetLoadBudget;

    #[test]
    fn test_basic_functionality() {
        let _doc = YamlDocument::from_entries(Vec::new());
    }

    #[test]
    fn budgeted_parser_is_available_from_the_crate_root() {
        let encoded: std::sync::Arc<[u8]> = std::sync::Arc::from(b"root: value\n".as_slice());
        let mut budget = AssetLoadBudget::default();
        let parsed: std::result::Result<BudgetedYamlSource, BudgetedYamlError> =
            parse_budgeted_yaml_source(encoded, &mut budget);

        let parsed = parsed.unwrap();
        assert_eq!(parsed.document().entries().len(), 1);
        assert_eq!(parsed.inspection().encoded_bytes(), 12);
        assert_eq!(parsed.inspection().documents(), 1);
        assert!(parsed.inspection().events() >= 7);
        assert_eq!(parsed.inspection().max_depth(), 1);
    }
}
