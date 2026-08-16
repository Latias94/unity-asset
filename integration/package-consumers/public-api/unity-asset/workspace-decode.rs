//! Workspace decoding feature contract documented for external consumers.

pub use unity_asset::{
    AssetLoadBudget, ObjectAddress, WorkspaceRevision,
    extraction::{ExtractionPlan, ExtractionPlanner, ExtractionRequest, MediaInspectionError},
    workspace::{AssetWorkspace, WorkspaceInspector, WorkspaceSnapshot, WorkspaceView},
};
