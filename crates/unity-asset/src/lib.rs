//! Revisioned Unity asset workspace.
//!
//! `unity-asset` is the public aggregation layer for loading Unity YAML, SerializedFiles,
//! AssetBundles, WebFiles, archives, and streamed resources into one immutable, revision-bound
//! object model.
//!
//! [`workspace::AssetWorkspace`] owns source identity and committed state. Read operations use
//! immutable [`workspace::WorkspaceSnapshot`] or [`workspace::PreparedView`] values through
//! [`workspace::WorkspaceView`]. Mutations are inert [`workspace::MutationPlan`] data until
//! `prepare` proves a candidate revision; `commit` consumes that one-use authority.
//!
//! # Inspect a source
//!
//! ```rust,no_run
//! use unity_asset::AssetLoadBudget;
//! use unity_asset::workspace::{AssetWorkspace, WorkspaceInspector};
//!
//! let mut budget = AssetLoadBudget::default();
//! let mut workspace = AssetWorkspace::new()?;
//! workspace.load_path("game.bundle", &mut budget)?;
//!
//! let snapshot = workspace.snapshot();
//! let inspector = WorkspaceInspector::new(&snapshot);
//! for source in inspector.sources(&mut budget)? {
//!     println!("{:?}", source.source().locator());
//! }
//!
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Capability discovery
//!
//! Automation should inspect [`workspace::workspace_capabilities`] and exchange versioned
//! `ObjectAddress`, mutation, recovery, reference, and extraction contracts. Display output is
//! diagnostic text, not a persistence format.
//!
//! # Crate boundaries
//!
//! Format-local parsers remain available from `unity-asset-yaml` and `unity-asset-binary`.
//! Wire encoders live in `unity-asset-write`; optional media conversion lives in
//! `unity-asset-decode`. Those crates do not replace workspace transaction authority.

// Re-export from core crate
pub use unity_asset_core::{
    AssetLoadBudget, AssetLoadBudgetDomainToken, AssetLoadDepthScope, AssetLoadLimits,
    AssetLoadUsage, BudgetError, BudgetedJsonError, BudgetedSourceBytes, BundleMemberId,
    CHANGE_SET_VERSION, ChangeSet, ChangeSetError, ContainmentKind, ContractError,
    ContractJsonLimits, ContractJsonResourceModel, DIAGNOSTIC_VERSION, DecompressionBudget,
    DecompressionUsage, Diagnostic, DiagnosticError, DiagnosticSeverity, DigestBuildError,
    DigestParseError, DigestV1, DigestV1Builder, DocumentFormat, FieldPath, FieldPathError,
    FieldPathSegment, IdentityRemap, ObjectAddress, ObjectId, ObjectKind, Result,
    RevisionedObjectHandle, SourceAlias, SourceFingerprint, SourceId, SourceKind, SourceLocator,
    SourceMemberId, TransactionId, UnityAssetError, UnityClass, UnityClassHeader, UnityDocument,
    UnityValue, WorkspaceId, WorkspaceRevision, YamlDocumentSelector, YamlFileId, constants::*,
    read_contract_json, read_contract_json_slice,
};

pub use unity_asset_core::get_class_name;
pub use unity_asset_core::get_class_name_str;

// Re-export the wire model used by workspace inspection.
pub use unity_asset_binary::asset::SerializedFile;
pub use unity_asset_binary::error::{BinaryError, BinaryObjectIdentityError};

// Re-export async traits when async feature is enabled
#[cfg(feature = "async")]
pub use unity_asset_core::document::AsyncUnityDocument;

pub mod extraction;
pub mod reference;
pub mod schema;
pub mod workspace;
