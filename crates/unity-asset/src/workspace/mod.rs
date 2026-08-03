//! Revisioned ownership and source-resolution foundation.

mod adapter;
mod capabilities;
pub(crate) mod commit;
mod inspection;
mod interface;
mod overlay;
mod plan;
mod portable_path;
mod preflight;
mod snapshot;
mod source_admission;
mod source_catalog;
mod state;
mod view;

pub use capabilities::{
    WORKSPACE_CAPABILITY_CATALOG_CONTRACT, WORKSPACE_CAPABILITY_CATALOG_VERSION,
    WorkspaceAutomationCapability, WorkspaceCapability, WorkspaceCapabilityCatalog,
    WorkspaceContractVersions, WorkspaceInspectionCapability, WorkspaceMutationCapability,
    WorkspaceMutationFamily, WorkspacePreparedAuthorityCapability, WorkspacePublicationCapability,
    WorkspaceSearchHandoffArtifact, WorkspaceSearchHandoffCapability, WorkspaceViewCapability,
    WorkspaceViewKind, workspace_capabilities,
};
pub use commit::{
    COMMIT_REPORT_VERSION, CommitArtifactReport, CommitAtomicity, CommitContractError,
    CommitDestinationState, CommitError, CommitReport, PublicationTarget, PublicationTargetError,
    RECOVERY_DISCOVERY_VERSION, RECOVERY_LOCATOR_VERSION, RECOVERY_OUTCOME_VERSION,
    ROLLBACK_RECEIPT_VERSION, RecoveryBlockedReason, RecoveryDiscovery,
    RecoveryDiscoveryBlockedReason, RecoveryDiscoveryError, RecoveryError, RecoveryLocator,
    RecoveryOutcome, RollbackReceipt,
};
pub use inspection::{
    AssetBundleSummary, ResolvedStreamedResource, STREAMED_RESOURCE_QUERY_VERSION,
    SerializedFileSummary, SerializedPathIdSummary, StreamedResourceCandidate,
    StreamedResourceQueryResult, StreamedResourceRequest, StreamedResourceRequestError,
    StreamedResourceResolution, WORKSPACE_OBJECT_INSPECTION_VERSION,
    WORKSPACE_SOURCE_INSPECTION_VERSION, WebFileSummary, WorkspaceBundleLayout, WorkspaceByteOrder,
    WorkspaceCompression, WorkspaceInspector, WorkspaceObjectFormatInspection,
    WorkspaceObjectInspection, WorkspaceSourceFormatInspection, WorkspaceSourceInspection,
};
pub use interface::{AssetWorkspace, SourceOpenRequest, WorkspaceOptions};
pub use overlay::PreparedView;
pub use plan::{
    FieldGuard, Float64Bits, GenericMutation, MUTATION_PLAN_VERSION, MutationField,
    MutationOperation, MutationPlan, MutationPlanBuilder, MutationPlanBuilderError,
    MutationPlanError, MutationPlanFragment, MutationPlanReadError, MutationValue,
    MutationValueRef, ObjectGuard, PlanBytes, PlanPayload, ReferenceTarget, SequenceMutation,
    SourceExpectation, UnsafeRawAcknowledgement,
};
pub use preflight::{
    PREPARE_REPORT_VERSION, PrepareArtifactReport, PrepareDiagnostic, PrepareError,
    PrepareFailureReport, PrepareOptions, PrepareReport, PrepareStage, PreparedChange,
    PreparedSourceReport,
};
pub use snapshot::WorkspaceSnapshot;
pub use source_admission::{
    SourceAdmissionBatch, SourceAdmissionBatchAllocationError, SourceAdmissionBatchCapacityError,
    SourceAdmissionBatchPhase, SourceAdmissionBatchPushError, SourceAdmissionDisposition,
    SourceAdmissionError, SourceAdmissionErrorCategory, SourceAdmissionFailure,
    SourceAdmissionFailureSite, SourceAdmissionOperation, SourceAdmissionOperationLocation,
    SourceAdmissionOutcome, SourceAdmissionPolicy, SourceAdmissionRejection, SourceAdmissionReport,
};
pub use source_catalog::SourceLocationKind;
pub(crate) use view::SourceObjectDescriptor;
pub use view::{
    WorkspaceAllocationUnit, WorkspaceByteRange, WorkspaceByteRangeReader, WorkspaceError,
    WorkspaceLookup, WorkspaceObject, WorkspaceObjectValue, WorkspaceSource,
    WorkspaceSourceContainer, WorkspaceSourceIdentityError, WorkspaceSourceMemberIdentityError,
    WorkspaceView, WorkspaceYamlObject,
};

#[cfg(any(feature = "decode", test))]
pub(crate) use inspection::StreamedResourceResolver;
#[cfg(test)]
pub(crate) use state::TestSourceBackingOwner;
pub use state::WorkspaceInstallationDigest;
pub(crate) use state::{WeakSourceBackingOwner, WorkspaceState};

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

pub(crate) fn object_count_in_source(
    view: &dyn WorkspaceView,
    source: unity_asset_core::SourceId,
    budget: &mut unity_asset_core::AssetLoadBudget,
) -> Result<usize, WorkspaceError> {
    view::sealed::Sealed::object_count_in_source(view, source, budget)
}

pub(crate) fn object_descriptor_at_in_source(
    view: &dyn WorkspaceView,
    source: unity_asset_core::SourceId,
    index: usize,
    budget: &mut unity_asset_core::AssetLoadBudget,
) -> Result<view::SourceObjectDescriptor, WorkspaceError> {
    view::sealed::Sealed::object_descriptor_at_in_source(view, source, index, budget)
}

pub(crate) fn read_object_at_in_source(
    view: &dyn WorkspaceView,
    descriptor: &view::SourceObjectDescriptor,
    budget: &mut unity_asset_core::AssetLoadBudget,
) -> Result<WorkspaceObject, WorkspaceError> {
    view::sealed::Sealed::read_object_at_in_source(view, descriptor, budget)
}
