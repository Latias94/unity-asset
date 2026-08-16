//! Public API contract for the default `unity-asset-core` package.

pub use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, AudioClipResourceField, AudioClipResourceSelection,
    AudioClipResourceShapeError, BudgetedVerifiedSourceImage, ChangeSet, Diagnostic, DigestV1,
    ObjectAddress, SourceFingerprint, SourceId, SourceLocator, StreamDataDeclaration,
    UnityDocument, UnityValue, WorkspaceId, WorkspaceRevision, YamlDocumentSelector, YamlFileId,
    classify_audio_clip_resource,
};
