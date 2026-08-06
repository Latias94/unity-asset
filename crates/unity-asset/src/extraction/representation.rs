mod contract;
#[cfg(feature = "decode")]
mod payload;
mod planning;
mod prepared;
mod reservation;
mod runtime;

#[cfg(feature = "decode")]
use unity_asset_binary::asset::SerializedObjectContext;

#[cfg(feature = "decode")]
use crate::workspace::{WorkspaceError, WorkspaceInspector, WorkspaceObject, WorkspaceView};

pub(in crate::extraction) use contract::{
    PlannedContent, PlannedFallback, RepresentationContract, RepresentationContractError,
    RepresentationContractParts, RepresentationSemantics,
};
pub(super) use planning::RepresentationPlanner;
pub(super) use prepared::{
    PreparedRepresentation, RepresentationPreparationError, RepresentationWriteError,
};
pub(super) use reservation::ExtractionReservationError;
#[cfg(not(feature = "decode"))]
pub(super) use reservation::{raw_binary_working_set, yaml_working_set};
pub(super) use runtime::{RepresentationRuntime, RepresentationRuntimeContext};

#[cfg(feature = "decode")]
fn texture_inspection_context(
    view: &dyn WorkspaceView,
    object: &WorkspaceObject,
) -> Result<SerializedObjectContext, WorkspaceError> {
    WorkspaceInspector::new(view).serialized_object_context(object.handle().object().source())
}
