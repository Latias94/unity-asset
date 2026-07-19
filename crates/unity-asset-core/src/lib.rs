//! Unity Asset Core
//!
//! Core data structures and types for Unity asset parsing.
//! This crate provides the fundamental building blocks that are shared
//! across different Unity asset formats (YAML, binary, etc.).

pub mod allocation;
mod bounded;
pub mod budget;
pub mod change;
pub mod constants;
pub mod diagnostic;
pub mod digest;
pub mod document;
pub mod dynamic_access;
pub mod error;
pub mod field_path;
pub mod identity;
pub mod revision;
pub mod semantic_digest;
pub mod source_image;
pub mod unity_class;
pub mod unity_value;

// Re-export main types
pub use allocation::{
    AllocationSizeError, arc_slice_allocation_bytes, arc_value_allocation_bytes,
    arc_vec_allocation_bytes, string_allocation_bytes, vec_allocation_bytes,
};
pub use budget::{
    AssetLoadBudget, AssetLoadDepthScope, AssetLoadLimits, AssetLoadUsage, BudgetError,
    BudgetedJsonError, DecompressionBudget, DecompressionUsage,
};
pub use change::{ChangeSet, ChangeSetError, IdentityRemap, TransactionId};
pub use constants::*;
pub use diagnostic::{Diagnostic, DiagnosticError, DiagnosticSeverity};
pub use digest::{DigestBuildError, DigestParseError, DigestV1, DigestV1Builder};
pub use document::{DocumentFormat, UnityDocument};
pub use dynamic_access::{DynamicAccess, DynamicValue};
pub use error::{Result, UnityAssetError};
pub use field_path::{FieldPath, FieldPathError, FieldPathSegment};
pub use identity::{
    BundleMemberId, ContainmentKind, ContainmentStep, ContractError, ObjectAddress, ObjectId,
    ObjectKind, RevisionedObjectHandle, SourceAlias, SourceId, SourceLocator, SourceMemberId,
    WorkspaceId, YamlAnchor, YamlDocumentSelector,
};
pub use revision::{SourceFingerprint, SourceKind, WorkspaceRevision};
pub use semantic_digest::{
    SemanticDigestError, field_schema_digest, observe_semantic_value, semantic_value_digest,
    yaml_field_schema_digest, yaml_schema_digest,
};
pub use source_image::{VerifiedSourceImage, VerifiedSourceImageError, VerifiedSourceRebinding};
pub use unity_class::{UnityClass, UnityClassRegistry};
pub use unity_value::UnityValue;

/// Get Unity class name from class ID
pub fn get_class_name(class_id: i32) -> Option<String> {
    GLOBAL_CLASS_ID_MAP.get_class_name(class_id)
}

/// Get Unity class name from class ID without allocating.
pub fn get_class_name_str(class_id: i32) -> Option<&'static str> {
    GLOBAL_CLASS_ID_MAP.get_class_name_str(class_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_functionality() {
        // Basic functionality test.
        let class = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
        assert_eq!(class.class_id, 1);
        assert_eq!(class.class_name, "GameObject");
    }
}
