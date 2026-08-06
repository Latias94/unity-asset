//! Shared contract and budgeted primitives for extraction planning.

use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ContractError, ObjectAddress, RevisionedObjectHandle,
    SourceFingerprint, SourceId, SourceLocator, vec_allocation_bytes,
};
#[cfg(feature = "decode")]
use unity_asset_decode::media::MediaInspectionError;

use super::contract::{ExtractionAllocationUnit, ExtractionDiagnosticCode};
use crate::reference::ReferenceGraphError;
use crate::workspace::{WorkspaceError, WorkspaceLookup, WorkspaceSource, WorkspaceView};

pub(in crate::extraction) fn usize_to_u64(
    value: usize,
    resource: &'static str,
) -> Result<u64, ExtractionPlanError> {
    u64::try_from(value).map_err(|_| ExtractionPlanError::ArithmeticOverflow { resource })
}

pub(in crate::extraction) fn clone_string(
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ExtractionPlanError> {
    let bytes = usize_to_u64(value.len(), resource)?;
    budget.check_bytes(bytes)?;
    let mut cloned = String::new();
    cloned
        .try_reserve_exact(value.len())
        .map_err(|source| ExtractionPlanError::Allocation {
            resource,
            requested: value.len(),
            unit: ExtractionAllocationUnit::Bytes,
            source,
        })?;
    let retained = usize_to_u64(cloned.capacity(), resource)?;
    budget.check_bytes(retained)?;
    cloned.push_str(value);
    budget.consume_bytes(retained)?;
    Ok(cloned)
}

pub(in crate::extraction) fn clone_source_locator(
    value: &SourceLocator,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<SourceLocator, ExtractionPlanError> {
    let bytes = value
        .retained_clone_bytes()
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let cloned = value.clone();
    budget.consume_bytes(bytes)?;
    Ok(cloned)
}

pub(in crate::extraction) fn clone_object_address(
    value: &ObjectAddress,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<ObjectAddress, ExtractionPlanError> {
    let bytes = value
        .retained_clone_bytes()
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let cloned = value.clone();
    budget.consume_bytes(bytes)?;
    Ok(cloned)
}

pub(in crate::extraction) fn budgeted_vec<T>(
    count: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, ExtractionPlanError> {
    let entries = u64::try_from(count).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    let minimum_bytes = vec_allocation_bytes::<T>(count)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_entries(entries)?;
    budget.check_bytes(minimum_bytes)?;

    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|source| ExtractionPlanError::Allocation {
            resource,
            requested: count,
            unit: ExtractionAllocationUnit::CapacityUnits,
            source,
        })?;
    let retained_bytes = vec_allocation_bytes::<T>(values.capacity())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(values)
}

pub(in crate::extraction) fn push_budgeted<T>(
    values: &mut Vec<T>,
    value: T,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), ExtractionPlanError> {
    budget.check_entries(1)?;
    let previous_bytes = vec_allocation_bytes::<T>(values.capacity())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    if values.len() == values.capacity() {
        let planned_capacity = values
            .capacity()
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow { resource })?;
        let planned_bytes = vec_allocation_bytes::<T>(planned_capacity)
            .map_err(|_| BudgetError::ArithmeticOverflow { resource })?
            .checked_sub(previous_bytes)
            .ok_or(BudgetError::ArithmeticOverflow { resource })?;
        budget.check_bytes(planned_bytes)?;
        values
            .try_reserve_exact(1)
            .map_err(|source| ExtractionPlanError::Allocation {
                resource,
                requested: 1,
                unit: ExtractionAllocationUnit::CapacityUnits,
                source,
            })?;
    }
    let retained_bytes = vec_allocation_bytes::<T>(values.capacity())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?
        .checked_sub(previous_bytes)
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_entries(1)?;
    budget.consume_bytes(retained_bytes)?;
    values.push(value);
    Ok(())
}

pub(in crate::extraction) fn resolve_required_handle(
    view: &dyn WorkspaceView,
    address: &ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<RevisionedObjectHandle, ExtractionPlanError> {
    match view.resolve_object(address, budget)? {
        WorkspaceLookup::Resolved(handle) => Ok(handle),
        WorkspaceLookup::Unloaded => Err(ExtractionPlanError::ObjectUnloaded(
            clone_object_address(address, "unloaded object address", budget)?,
        )),
        WorkspaceLookup::Missing => Err(ExtractionPlanError::ObjectMissing(clone_object_address(
            address,
            "missing object address",
            budget,
        )?)),
        WorkspaceLookup::Ambiguous { candidates } => Err(ExtractionPlanError::ObjectAmbiguous {
            address: clone_object_address(address, "ambiguous object address", budget)?,
            candidates: candidates.len(),
        }),
        WorkspaceLookup::Invalid { .. } => Err(ExtractionPlanError::ObjectInvalid(
            clone_object_address(address, "invalid object address", budget)?,
        )),
    }
}

pub(in crate::extraction) fn source_for_id(
    id: SourceId,
    sources: &[WorkspaceSource],
) -> Result<&WorkspaceSource, ExtractionPlanError> {
    sources
        .iter()
        .find(|source| source.id() == id)
        .ok_or(ExtractionPlanError::SourceMissing(id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractionPlanMismatchKind {
    SelectionWitness,
    SourceExpectations,
    Artifacts,
    ArtifactPaths,
    Representations,
}

impl std::fmt::Display for ExtractionPlanMismatchKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SelectionWitness => "selection witness",
            Self::SourceExpectations => "source expectations",
            Self::Artifacts => "artifact contracts",
            Self::ArtifactPaths => "artifact paths",
            Self::Representations => "representations",
        })
    }
}

#[derive(Debug, Error)]
pub enum ExtractionPlanError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Reference(Box<ReferenceGraphError>),
    #[error(transparent)]
    ContainerContract(#[from] super::container::BundleContainerContractError),
    #[error(transparent)]
    Diagnostic(#[from] unity_asset_core::DiagnosticError),
    #[error(transparent)]
    FieldPath(#[from] unity_asset_core::FieldPathError),
    #[error("failed to reserve {requested} {unit} for {resource}: {source}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        unit: ExtractionAllocationUnit,
        #[source]
        source: std::collections::TryReserveError,
    },
    #[error("extraction model rejected the plan: {0}")]
    Model(String),
    #[error(transparent)]
    ModelValidation(Box<super::model::ExtractionModelError>),
    #[cfg(feature = "decode")]
    #[error("strict media descriptor for {address:?} is invalid: {source}")]
    InvalidMediaDescriptor {
        address: ObjectAddress,
        source: MediaInspectionError,
    },
    #[error("strict media preparation failed for {address:?}")]
    MediaPreparation { address: ObjectAddress },
    #[error("failed to allocate {requested} bytes for strict media {resource}")]
    MediaAllocation {
        resource: &'static str,
        requested: usize,
    },
    #[error("strict media payload changed after inspection for {resource}")]
    MediaPayloadChanged { resource: &'static str },
    #[error("reference graph violated an extraction invariant: {0}")]
    ReferenceInvariant(&'static str),
    #[error("extraction plan does not describe this workspace revision")]
    PlanContextMismatch,
    #[error("extraction plan {kind} does not match its re-derived request")]
    PlanDerivationMismatch { kind: ExtractionPlanMismatchKind },
    #[error("planned artifact {ordinal} requires unavailable execution capability {capability}")]
    ExecutionCapabilityUnavailable {
        ordinal: u32,
        capability: &'static str,
    },
    #[error("class {class_id} planning requires unavailable capability {capability}")]
    PlanningCapabilityUnavailable {
        class_id: i32,
        capability: &'static str,
    },
    #[error("an incomplete reference graph cannot drive bundle-container extraction")]
    IncompleteReferenceGraph,
    #[error("an incomplete reference traversal cannot be used as an extraction selection")]
    IncompleteReferenceTraversal,
    #[error("required decoded representation is unavailable for {address:?}: {reason:?}")]
    RequiredDecodedUnavailable {
        address: ObjectAddress,
        reason: ExtractionDiagnosticCode,
    },
    #[error("object is not loaded: {0:?}")]
    ObjectUnloaded(ObjectAddress),
    #[error("object does not exist: {0:?}")]
    ObjectMissing(ObjectAddress),
    #[error("object address {address:?} is ambiguous across {candidates} candidates")]
    ObjectAmbiguous {
        address: ObjectAddress,
        candidates: usize,
    },
    #[error("object address is invalid: {0:?}")]
    ObjectInvalid(ObjectAddress),
    #[error("workspace source is missing: {0:?}")]
    SourceMissing(SourceId),
    #[error("source {locator:?} has conflicting fingerprints {first} and {second}")]
    SourceFingerprintConflict {
        locator: Box<SourceLocator>,
        first: SourceFingerprint,
        second: SourceFingerprint,
    },
    #[error("stream source is missing: {0:?}")]
    StreamSourceMissing(SourceLocator),
    #[error("invalid streamed resource path: {0:?}")]
    InvalidStreamPath(String),
    #[error("invalid streamed resource range: offset={offset}, size={size}")]
    InvalidStreamRange { offset: u64, size: u64 },
    #[error("streamed resource {stream_path:?} is missing for {owner:?}")]
    MissingStreamResource {
        owner: SourceLocator,
        stream_path: String,
    },
    #[error("streamed resource {stream_path:?} is ambiguous for {owner:?}")]
    AmbiguousStreamResource {
        owner: SourceLocator,
        stream_path: String,
    },
    #[error("failed to encode canonical object address: {0}")]
    CanonicalAddress(String),
    #[error("failed to format extraction output path")]
    PathFormatting,
    #[error("failed to measure canonical YAML output: {0}")]
    YamlSizing(String),
    #[error("arithmetic overflow while planning {resource}")]
    ArithmeticOverflow { resource: &'static str },
}

impl From<super::model::ExtractionModelError> for ExtractionPlanError {
    fn from(error: super::model::ExtractionModelError) -> Self {
        Self::ModelValidation(Box::new(error))
    }
}

impl From<ReferenceGraphError> for ExtractionPlanError {
    fn from(error: ReferenceGraphError) -> Self {
        Self::Reference(Box::new(error))
    }
}
