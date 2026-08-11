//! Public API contract for the default `unity-asset-core` package.

pub use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, BudgetedVerifiedSourceImage, ChangeSet, Diagnostic,
    DigestV1, ObjectAddress, SourceFingerprint, SourceId, SourceLocator, UnityDocument,
    UnityValue, WorkspaceId, WorkspaceRevision, YamlDocumentSelector, YamlFileId,
};
