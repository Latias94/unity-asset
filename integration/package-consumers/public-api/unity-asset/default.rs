//! Public API contract for the default `unity-asset` package.

pub use unity_asset::{
    AssetLoadBudget, ChangeSet, DigestV1, ObjectAddress, SourceId, SourceLocator, UnityDocument,
    WorkspaceId, WorkspaceRevision,
    extraction::{
        ExtractionExecutor, ExtractionPlan, ExtractionPlanner, ExtractionRepresentationPolicy,
        ExtractionRequest,
    },
    reference::{ReferenceFact, ReferenceGraphBuildOptions, ReferenceResolution},
    workspace::{
        AssetWorkspace, MutationPlan, MutationPlanBuilder, PreparedChange, SourceAdmissionBatch,
        SourceAdmissionOperation, SourceAdmissionPolicy, SourceCompanionRequest, SourceOpenRequest,
        WorkspaceInspector, WorkspaceOptions, WorkspaceSnapshot, WorkspaceView,
        workspace_capabilities,
    },
};
