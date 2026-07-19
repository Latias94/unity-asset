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
    WorkspaceAllocationUnit, WorkspaceByteRange, WorkspaceByteRangeReader, WorkspaceError,
    WorkspaceLookup, WorkspaceObject, WorkspaceObjectValue, WorkspaceSource,
    WorkspaceSourceContainer, WorkspaceSourceIdentityError, WorkspaceSourceMemberIdentityError,
    WorkspaceView, WorkspaceYamlObject,
};

pub(crate) use state::WorkspaceState;
pub(crate) use store::SourceEntry;

#[doc(hidden)]
pub struct ReferenceViewParts<'a> {
    pub(crate) state: &'a std::sync::Arc<WorkspaceState>,
    pub(crate) store: &'a std::sync::Arc<crate::reference::ReferenceStore>,
    pub(crate) typetree: unity_asset_binary::typetree::TypeTreeParseOptions,
}

pub(crate) fn reference_view_parts(view: &dyn WorkspaceView) -> ReferenceViewParts<'_> {
    view::sealed::Sealed::reference_view_parts(view)
}
