//! Revisioned ownership and source-resolution foundation.

mod adapter;
mod interface;
mod plan;
mod snapshot;
mod source_catalog;
mod state;
mod store;
mod view;

pub use interface::{AssetWorkspace, SourceOpenRequest, WorkspaceOptions};
pub use plan::{
    FieldGuard, Float64Bits, GenericMutation, MutationField, MutationOperation, MutationPlan,
    MutationPlanBuilder, MutationPlanBuilderError, MutationPlanError, MutationPlanFragment,
    MutationPlanReadError, MutationValue, MutationValueRef, ObjectGuard, PlanBytes, PlanPayload,
    ReferenceTarget, SequenceMutation, SourceExpectation, UnsafeRawAcknowledgement,
};
pub use snapshot::WorkspaceSnapshot;
pub use source_catalog::SourceLocationKind;
pub use view::{
    WorkspaceAllocationUnit, WorkspaceBytes, WorkspaceError, WorkspaceLookup, WorkspaceObject,
    WorkspaceObjectValue, WorkspaceSource, WorkspaceSourceContainer, WorkspaceSourceIdentityError,
    WorkspaceSourceMemberIdentityError, WorkspaceView, WorkspaceYamlObject,
};
