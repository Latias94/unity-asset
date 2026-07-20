//! Revisioned ownership and source-resolution foundation.

mod adapter;
mod interface;
mod overlay;
mod plan;
mod preflight;
mod snapshot;
mod source_catalog;
mod state;
mod store;
mod view;

pub use interface::{AssetWorkspace, SourceOpenRequest, WorkspaceOptions};
pub use overlay::PreparedView;
pub use plan::{
    FieldGuard, Float64Bits, GenericMutation, MutationField, MutationOperation, MutationPlan,
    MutationPlanBuilder, MutationPlanBuilderError, MutationPlanError, MutationPlanFragment,
    MutationPlanReadError, MutationValue, MutationValueRef, ObjectGuard, PlanBytes, PlanPayload,
    ReferenceTarget, SequenceMutation, SourceExpectation, UnsafeRawAcknowledgement,
};
pub use preflight::{
    PREPARE_REPORT_VERSION, PrepareArtifactReport, PrepareDiagnostic, PrepareError,
    PrepareFailureReport, PrepareOptions, PrepareReport, PrepareStage, PreparedChange,
    PreparedSourceReport,
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

#[doc(hidden)]
pub struct ReferenceViewParts<'a> {
    pub(crate) state: ReferenceViewState<'a>,
    pub(crate) store: &'a std::sync::Arc<crate::reference::ReferenceStore>,
    pub(crate) typetree: unity_asset_binary::typetree::TypeTreeParseOptions,
}

#[derive(Clone, Copy)]
pub(crate) enum ReferenceViewState<'a> {
    Committed(&'a std::sync::Arc<WorkspaceState>),
    Prepared(&'a overlay::PreparedStateCore),
}

impl<'a> ReferenceViewParts<'a> {
    pub(crate) const fn committed(
        state: &'a std::sync::Arc<WorkspaceState>,
        store: &'a std::sync::Arc<crate::reference::ReferenceStore>,
        typetree: unity_asset_binary::typetree::TypeTreeParseOptions,
    ) -> Self {
        Self {
            state: ReferenceViewState::Committed(state),
            store,
            typetree,
        }
    }

    pub(crate) const fn prepared(
        state: &'a overlay::PreparedStateCore,
        store: &'a std::sync::Arc<crate::reference::ReferenceStore>,
        typetree: unity_asset_binary::typetree::TypeTreeParseOptions,
    ) -> Self {
        Self {
            state: ReferenceViewState::Prepared(state),
            store,
            typetree,
        }
    }
}

pub(crate) fn reference_view_parts(view: &dyn WorkspaceView) -> ReferenceViewParts<'_> {
    view::sealed::Sealed::reference_view_parts(view)
}
