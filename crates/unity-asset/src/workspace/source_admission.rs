//! Ordered, transactional admission of authoritative workspace sources.

use std::collections::{HashMap, TryReserveError};
use std::fmt;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;
use unity_asset_binary::error::BinaryError;
use unity_asset_core::{
    AssetLoadBudget, AssetLoadBudgetDomainToken, BudgetError, BudgetedSourceBytes, ContainmentKind,
    SourceAlias, SourceId, SourceKind, SourceMemberId, WorkspaceRevision, vec_allocation_bytes,
};
use unity_asset_yaml::BudgetedYamlError;

use super::adapter::archive::ArchiveLoadError;
use super::interface::AssetWorkspace;
use super::source_catalog::{
    CatalogError, PhysicalOrigin, PhysicalOriginError, RootAdmissionDecision, SourceDescriptor,
    VerifiedPhysicalBinding,
};
use super::source_loading::{
    prepare_root, prepared_raw, read_owned_image, reserve_budgeted_vec, usize_to_u64,
};
use super::state::{
    PreparedSourceChild, PreparedSourceRelation, PreparedSourceTree, WorkspaceStateInstallOutcome,
    WorkspaceStateTransaction,
};
use super::view::{WorkspaceAllocationUnit, WorkspaceError};

/// One explicit streamed-resource companion attached to a YAML or SerializedFile root.
#[derive(Debug, Clone)]
pub struct SourceCompanionRequest {
    path: PathBuf,
    member: SourceMemberId,
}

impl SourceCompanionRequest {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, member: SourceMemberId) -> Self {
        Self {
            path: path.into(),
            member,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn member(&self) -> &SourceMemberId {
        &self.member
    }

    fn retained_admission_bytes(&self) -> Result<u64, BudgetError> {
        self.path
            .capacity()
            .checked_add(self.member.retained_clone_bytes())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "source companion admission metadata",
            })
    }
}

/// One explicit filesystem source load.
#[derive(Debug, Clone)]
pub struct SourceOpenRequest {
    path: PathBuf,
    alias: SourceAlias,
    kind_hint: Option<SourceKind>,
    companions: Vec<SourceCompanionRequest>,
}

impl SourceOpenRequest {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, alias: SourceAlias) -> Self {
        Self {
            path: path.into(),
            alias,
            kind_hint: None,
            companions: Vec::new(),
        }
    }

    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        let path = path.into();
        let alias = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| WorkspaceError::InvalidSource {
                path: path.clone(),
                message: "the path has no portable UTF-8 file name; provide an explicit alias"
                    .to_owned(),
            })?
            .to_owned();
        Ok(Self::new(path, SourceAlias::new(alias)?))
    }

    #[must_use]
    pub fn with_kind_hint(mut self, kind: SourceKind) -> Self {
        self.kind_hint = Some(kind);
        self
    }

    /// Attaches one caller-discovered companion without asking the workspace to scan a directory.
    #[must_use]
    pub fn with_companion(mut self, companion: SourceCompanionRequest) -> Self {
        self.companions.push(companion);
        self
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn alias(&self) -> &SourceAlias {
        &self.alias
    }

    #[must_use]
    pub const fn kind_hint(&self) -> Option<SourceKind> {
        self.kind_hint
    }

    #[must_use]
    pub fn companions(&self) -> &[SourceCompanionRequest] {
        &self.companions
    }

    fn retained_admission_bytes(&self) -> Result<u64, BudgetError> {
        let root = self
            .path
            .capacity()
            .checked_add(self.alias.retained_owned_bytes())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "source admission operation metadata",
            })?;
        let companion_records = vec_allocation_bytes::<SourceCompanionRequest>(
            self.companions.capacity(),
        )
        .map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "source admission operation metadata",
        })?;
        self.companions.iter().try_fold(
            root.checked_add(companion_records)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "source admission operation metadata",
                })?,
            |total, companion| {
                total
                    .checked_add(companion.retained_admission_bytes()?)
                    .ok_or(BudgetError::ArithmeticOverflow {
                        resource: "source admission operation metadata",
                    })
            },
        )
    }
}

enum SourceImageInput {
    Path,
    Unaccounted(Arc<[u8]>),
    Budgeted(BudgetedSourceBytes),
}

/// Failure policy for one ordered source-admission batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAdmissionPolicy {
    /// Any operation failure rejects the complete batch.
    Strict,
    /// Content, unsupported-format, and source-identity failures become ordered rejections.
    ///
    /// I/O, allocation, budget, source-change, contract, and resource-limit failures always
    /// reject the complete batch.
    TolerantContent,
}

impl SourceAdmissionPolicy {
    #[must_use]
    pub const fn tolerates(self, category: SourceAdmissionErrorCategory) -> bool {
        matches!(self, Self::TolerantContent)
            && matches!(
                category,
                SourceAdmissionErrorCategory::Content
                    | SourceAdmissionErrorCategory::Unsupported
                    | SourceAdmissionErrorCategory::Identity
                    | SourceAdmissionErrorCategory::DuplicateAlias
                    | SourceAdmissionErrorCategory::DuplicatePhysicalOrigin
            )
    }
}

/// One caller-ordered source admission operation.
pub enum SourceAdmissionOperation {
    /// Opens, verifies, and parses the bytes currently stored at the request path.
    LoadPath(SourceOpenRequest),
    /// Parses caller-owned bytes while retaining the request path as the physical origin.
    LoadBytes {
        request: SourceOpenRequest,
        image: Arc<[u8]>,
    },
    /// Transfers already-accounted bytes from the same load-budget domain.
    LoadBudgetedBytes {
        request: SourceOpenRequest,
        image: BudgetedSourceBytes,
    },
    /// Removes one loaded root and its complete contained subtree.
    Unload(SourceId),
}

impl fmt::Debug for SourceAdmissionOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoadPath(request) => formatter.debug_tuple("LoadPath").field(request).finish(),
            Self::LoadBytes { request, image } => formatter
                .debug_struct("LoadBytes")
                .field("request", request)
                .field("image_len", &image.len())
                .finish(),
            Self::LoadBudgetedBytes { request, image } => formatter
                .debug_struct("LoadBudgetedBytes")
                .field("request", request)
                .field("image_len", &image.as_bytes().len())
                .finish(),
            Self::Unload(source) => formatter.debug_tuple("Unload").field(source).finish(),
        }
    }
}

impl SourceAdmissionOperation {
    fn retained_admission_bytes(&self) -> Result<u64, BudgetError> {
        match self {
            Self::LoadPath(request)
            | Self::LoadBytes { request, .. }
            | Self::LoadBudgetedBytes { request, .. } => request.retained_admission_bytes(),
            Self::Unload(_) => Ok(0),
        }
    }
}

/// Caller-owned ordered operation collection.
pub struct SourceAdmissionBatch {
    operations: Vec<SourceAdmissionOperation>,
    domain: AssetLoadBudgetDomainToken,
}

impl fmt::Debug for SourceAdmissionBatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceAdmissionBatch")
            .field("operations", &self.operations.len())
            .field("capacity", &self.operations.capacity())
            .finish()
    }
}

impl SourceAdmissionBatch {
    /// Reserves the complete caller-owned operation collection in the supplied budget domain.
    pub fn with_capacity(
        capacity: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SourceAdmissionBatchAllocationError> {
        const RESOURCE: &str = "source admission batch";

        let planned_bytes = vec_allocation_bytes::<SourceAdmissionOperation>(capacity)
            .map_err(|_| BudgetError::ArithmeticOverflow { resource: RESOURCE })?;
        budget.check_bytes(planned_bytes)?;

        let mut operations = Vec::new();
        operations.try_reserve_exact(capacity).map_err(|error| {
            SourceAdmissionBatchAllocationError::Allocation {
                requested: capacity,
                source: error,
            }
        })?;
        let retained_bytes =
            vec_allocation_bytes::<SourceAdmissionOperation>(operations.capacity())
                .map_err(|_| BudgetError::ArithmeticOverflow { resource: RESOURCE })?;
        budget.check_bytes(retained_bytes)?;
        budget.consume_bytes(retained_bytes)?;
        Ok(Self {
            operations,
            domain: budget.domain_token(),
        })
    }

    /// Appends into the already-accounted capacity without allocating.
    pub fn try_push(
        &mut self,
        operation: SourceAdmissionOperation,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), SourceAdmissionBatchPushError> {
        let requested =
            self.operations
                .len()
                .checked_add(1)
                .ok_or(SourceAdmissionBatchCapacityError {
                    capacity: self.operations.capacity(),
                    requested: usize::MAX,
                })?;
        if requested > self.operations.capacity() {
            return Err(SourceAdmissionBatchCapacityError {
                capacity: self.operations.capacity(),
                requested,
            }
            .into());
        }
        self.validate_budget(budget)?;
        let retained_bytes = operation.retained_admission_bytes()?;
        budget.check_entries(1)?;
        budget.check_bytes(retained_bytes)?;
        budget.consume_entries(1)?;
        budget.consume_bytes(retained_bytes)?;
        self.operations.push(operation);
        Ok(())
    }

    #[must_use]
    pub fn operations(&self) -> &[SourceAdmissionOperation] {
        &self.operations
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.operations.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.operations.capacity()
    }

    pub(crate) fn into_operations(self) -> Vec<SourceAdmissionOperation> {
        self.operations
    }

    pub(crate) fn validate_budget(&self, budget: &AssetLoadBudget) -> Result<(), BudgetError> {
        self.domain.validate(budget, "source admission batch")
    }
}

#[derive(Debug, Error)]
pub enum SourceAdmissionBatchAllocationError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("failed to reserve an admission batch for {requested} operations")]
    Allocation {
        requested: usize,
        #[source]
        source: TryReserveError,
    },
}

#[derive(Debug, Error)]
pub enum SourceAdmissionBatchPushError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Capacity(#[from] SourceAdmissionBatchCapacityError),
}

impl SourceAdmissionBatchPushError {
    #[must_use]
    pub const fn capacity_error(&self) -> Option<&SourceAdmissionBatchCapacityError> {
        match self {
            Self::Budget(_) => None,
            Self::Capacity(error) => Some(error),
        }
    }
}

impl SourceAdmissionBatchAllocationError {
    #[must_use]
    pub const fn requested(&self) -> Option<usize> {
        match self {
            Self::Budget(_) => None,
            Self::Allocation { requested, .. } => Some(*requested),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("source admission batch capacity {capacity} cannot accept operation {requested}")]
pub struct SourceAdmissionBatchCapacityError {
    capacity: usize,
    requested: usize,
}

impl SourceAdmissionBatchCapacityError {
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    #[must_use]
    pub const fn requested(&self) -> usize {
        self.requested
    }
}

/// Stable, typed failure category for policy decisions and automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAdmissionErrorCategory {
    Content,
    Unsupported,
    Identity,
    DuplicateAlias,
    DuplicatePhysicalOrigin,
    Budget,
    Io,
    Allocation,
    SourceChanged,
    Contract,
    ResourceLimit,
    WorkspaceInvariant,
}

/// Stable batch phase for failures that cannot honestly be assigned to one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAdmissionBatchPhase {
    Preparation,
    CandidateApplication,
    Publication,
}

impl fmt::Display for SourceAdmissionBatchPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Preparation => "preparation",
            Self::CandidateApplication => "candidate application",
            Self::Publication => "publication",
        })
    }
}

/// Caller-facing identity for the operation that produced a failure or rejection.
#[derive(Debug, PartialEq, Eq)]
pub enum SourceAdmissionOperationLocation {
    Alias(SourceAlias),
    RequestPath(PathBuf),
    PhysicalOrigin(PathBuf),
    Source(SourceId),
}

impl SourceAdmissionOperationLocation {
    #[must_use]
    pub const fn alias(&self) -> Option<&SourceAlias> {
        match self {
            Self::Alias(alias) => Some(alias),
            Self::RequestPath(_) | Self::PhysicalOrigin(_) | Self::Source(_) => None,
        }
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::RequestPath(path) | Self::PhysicalOrigin(path) => Some(path),
            Self::Alias(_) | Self::Source(_) => None,
        }
    }

    #[must_use]
    pub const fn source_id(&self) -> Option<SourceId> {
        match self {
            Self::Source(source) => Some(*source),
            Self::Alias(_) | Self::RequestPath(_) | Self::PhysicalOrigin(_) => None,
        }
    }
}

impl fmt::Display for SourceAdmissionOperationLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Alias(alias) => write!(formatter, "alias {alias:?}"),
            Self::RequestPath(path) => write!(formatter, "request path {path:?}"),
            Self::PhysicalOrigin(path) => write!(formatter, "physical origin {path:?}"),
            Self::Source(source) => write!(formatter, "source {source:?}"),
        }
    }
}

/// Exact failure location for automation and diagnostics.
#[derive(Debug, PartialEq, Eq)]
pub enum SourceAdmissionFailureSite {
    Operation {
        ordinal: u64,
        location: Option<SourceAdmissionOperationLocation>,
    },
    Batch {
        phase: SourceAdmissionBatchPhase,
    },
}

impl SourceAdmissionFailureSite {
    #[must_use]
    pub const fn operation_ordinal(&self) -> Option<u64> {
        match self {
            Self::Operation { ordinal, .. } => Some(*ordinal),
            Self::Batch { .. } => None,
        }
    }

    #[must_use]
    pub const fn operation_location(&self) -> Option<&SourceAdmissionOperationLocation> {
        match self {
            Self::Operation { location, .. } => location.as_ref(),
            Self::Batch { .. } => None,
        }
    }

    #[must_use]
    pub const fn batch_phase(&self) -> Option<SourceAdmissionBatchPhase> {
        match self {
            Self::Operation { .. } => None,
            Self::Batch { phase } => Some(*phase),
        }
    }

    #[must_use]
    pub fn into_operation_location(self) -> Option<SourceAdmissionOperationLocation> {
        match self {
            Self::Operation { location, .. } => location,
            Self::Batch { .. } => None,
        }
    }
}

impl fmt::Display for SourceAdmissionFailureSite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Operation {
                ordinal,
                location: Some(location),
            } => write!(formatter, "operation {ordinal} ({location})"),
            Self::Operation {
                ordinal,
                location: None,
            } => write!(formatter, "operation {ordinal}"),
            Self::Batch { phase } => write!(formatter, "batch {phase}"),
        }
    }
}

/// Typed source-admission failure. Display text is diagnostic only.
#[derive(Debug, Error)]
pub enum SourceAdmissionFailure {
    #[error(transparent)]
    Workspace(Box<WorkspaceError>),
    #[error("companion sources require a filesystem-backed LoadPath operation")]
    CompanionsRequireFilesystemLoad,
    #[error("source kind {actual:?} cannot own filesystem companion sources")]
    InvalidCompanionParentKind { actual: SourceKind },
    #[error(
        "filesystem companion {member:?} uses same-name occurrence {same_name_occurrence}; filesystem companions require occurrence zero"
    )]
    InvalidCompanionOccurrence {
        member: SourceMemberId,
        same_name_occurrence: u32,
    },
    #[error("filesystem companion does not resolve to its declared sibling {expected:?}")]
    CompanionPathMismatch { expected: PathBuf },
    #[error("source alias duplicates operation {first_operation}")]
    DuplicateAlias { first_operation: u64 },
    #[error("physical origin duplicates operation {first_operation}")]
    DuplicatePhysicalOrigin { first_operation: u64 },
    #[error("source alias is already bound to {existing_source:?}")]
    AliasConflict { existing_source: SourceId },
    #[error("physical origin is already bound to {existing_source:?}")]
    PhysicalOriginConflict { existing_source: SourceId },
}

impl From<WorkspaceError> for SourceAdmissionFailure {
    fn from(error: WorkspaceError) -> Self {
        Self::Workspace(Box::new(error))
    }
}

impl SourceAdmissionFailure {
    #[must_use]
    pub fn category(&self) -> SourceAdmissionErrorCategory {
        match self {
            Self::Workspace(error) => classify_workspace_error(error),
            Self::CompanionsRequireFilesystemLoad | Self::InvalidCompanionParentKind { .. } => {
                SourceAdmissionErrorCategory::Contract
            }
            Self::InvalidCompanionOccurrence { .. } | Self::CompanionPathMismatch { .. } => {
                SourceAdmissionErrorCategory::Identity
            }
            Self::DuplicateAlias { .. } => SourceAdmissionErrorCategory::DuplicateAlias,
            Self::DuplicatePhysicalOrigin { .. } => {
                SourceAdmissionErrorCategory::DuplicatePhysicalOrigin
            }
            Self::AliasConflict { .. } | Self::PhysicalOriginConflict { .. } => {
                SourceAdmissionErrorCategory::Identity
            }
        }
    }

    #[must_use]
    pub const fn first_operation(&self) -> Option<u64> {
        match self {
            Self::Workspace(_)
            | Self::CompanionsRequireFilesystemLoad
            | Self::InvalidCompanionParentKind { .. }
            | Self::InvalidCompanionOccurrence { .. }
            | Self::CompanionPathMismatch { .. } => None,
            Self::DuplicateAlias {
                first_operation, ..
            }
            | Self::DuplicatePhysicalOrigin {
                first_operation, ..
            } => Some(*first_operation),
            Self::AliasConflict { .. } | Self::PhysicalOriginConflict { .. } => None,
        }
    }
}

/// One policy-approved rejection retained in a successful tolerant report.
#[derive(Debug)]
pub struct SourceAdmissionRejection {
    location: Option<SourceAdmissionOperationLocation>,
    failure: SourceAdmissionFailure,
}

impl SourceAdmissionRejection {
    pub(crate) const fn new(
        location: Option<SourceAdmissionOperationLocation>,
        failure: SourceAdmissionFailure,
    ) -> Self {
        Self { location, failure }
    }

    #[must_use]
    pub const fn operation_location(&self) -> Option<&SourceAdmissionOperationLocation> {
        self.location.as_ref()
    }

    #[must_use]
    pub fn category(&self) -> SourceAdmissionErrorCategory {
        self.failure.category()
    }

    #[must_use]
    pub const fn failure(&self) -> &SourceAdmissionFailure {
        &self.failure
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        Option<SourceAdmissionOperationLocation>,
        SourceAdmissionFailure,
    ) {
        (self.location, self.failure)
    }
}

/// Successful disposition of one input operation.
#[derive(Debug)]
pub enum SourceAdmissionDisposition {
    Loaded { source_id: SourceId },
    Unchanged { source_id: SourceId },
    Unloaded { source_id: SourceId },
    Rejected(SourceAdmissionRejection),
}

impl SourceAdmissionDisposition {
    #[must_use]
    pub const fn source_id(&self) -> Option<SourceId> {
        match self {
            Self::Loaded { source_id }
            | Self::Unchanged { source_id }
            | Self::Unloaded { source_id } => Some(*source_id),
            Self::Rejected(_) => None,
        }
    }

    #[must_use]
    pub const fn rejection(&self) -> Option<&SourceAdmissionRejection> {
        match self {
            Self::Rejected(rejection) => Some(rejection),
            Self::Loaded { .. } | Self::Unchanged { .. } | Self::Unloaded { .. } => None,
        }
    }
}

/// One disposition paired with its stable input ordinal.
#[derive(Debug)]
pub struct SourceAdmissionOutcome {
    operation_ordinal: u64,
    disposition: SourceAdmissionDisposition,
}

impl SourceAdmissionOutcome {
    pub(crate) const fn new(
        operation_ordinal: u64,
        disposition: SourceAdmissionDisposition,
    ) -> Self {
        Self {
            operation_ordinal,
            disposition,
        }
    }

    #[must_use]
    pub const fn operation_ordinal(&self) -> u64 {
        self.operation_ordinal
    }

    #[must_use]
    pub const fn disposition(&self) -> &SourceAdmissionDisposition {
        &self.disposition
    }

    #[must_use]
    pub fn into_disposition(self) -> SourceAdmissionDisposition {
        self.disposition
    }
}

/// Successful batch report. Every input has exactly one outcome in input order.
#[derive(Debug)]
pub struct SourceAdmissionReport {
    policy: SourceAdmissionPolicy,
    base_revision: WorkspaceRevision,
    revision: WorkspaceRevision,
    state_installed: bool,
    outcomes: Vec<SourceAdmissionOutcome>,
}

impl SourceAdmissionReport {
    pub(crate) const fn new(
        policy: SourceAdmissionPolicy,
        base_revision: WorkspaceRevision,
        revision: WorkspaceRevision,
        state_installed: bool,
        outcomes: Vec<SourceAdmissionOutcome>,
    ) -> Self {
        Self {
            policy,
            base_revision,
            revision,
            state_installed,
            outcomes,
        }
    }

    #[must_use]
    pub const fn policy(&self) -> SourceAdmissionPolicy {
        self.policy
    }

    #[must_use]
    pub const fn base_revision(&self) -> WorkspaceRevision {
        self.base_revision
    }

    #[must_use]
    pub const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub const fn state_installed(&self) -> bool {
        self.state_installed
    }

    #[must_use]
    pub fn outcomes(&self) -> &[SourceAdmissionOutcome] {
        &self.outcomes
    }

    #[must_use]
    pub fn into_outcomes(self) -> Vec<SourceAdmissionOutcome> {
        self.outcomes
    }
}

/// Fatal or strict-policy failure. No workspace state was installed.
#[derive(Debug, Error)]
#[error("source admission {site} failed: {failure}")]
pub struct SourceAdmissionError {
    site: SourceAdmissionFailureSite,
    #[source]
    failure: Box<SourceAdmissionFailure>,
}

impl SourceAdmissionError {
    pub(crate) fn operation(
        operation_ordinal: u64,
        location: Option<SourceAdmissionOperationLocation>,
        failure: SourceAdmissionFailure,
    ) -> Self {
        Self {
            site: SourceAdmissionFailureSite::Operation {
                ordinal: operation_ordinal,
                location,
            },
            failure: Box::new(failure),
        }
    }

    pub(crate) fn batch(phase: SourceAdmissionBatchPhase, failure: SourceAdmissionFailure) -> Self {
        Self {
            site: SourceAdmissionFailureSite::Batch { phase },
            failure: Box::new(failure),
        }
    }

    #[must_use]
    pub const fn site(&self) -> &SourceAdmissionFailureSite {
        &self.site
    }

    #[must_use]
    pub const fn operation_ordinal(&self) -> Option<u64> {
        self.site.operation_ordinal()
    }

    #[must_use]
    pub const fn operation_location(&self) -> Option<&SourceAdmissionOperationLocation> {
        self.site.operation_location()
    }

    #[must_use]
    pub const fn batch_phase(&self) -> Option<SourceAdmissionBatchPhase> {
        self.site.batch_phase()
    }

    #[must_use]
    pub fn category(&self) -> SourceAdmissionErrorCategory {
        self.failure.category()
    }

    #[must_use]
    pub fn failure(&self) -> &SourceAdmissionFailure {
        self.failure.as_ref()
    }

    #[must_use]
    pub fn into_parts(self) -> (SourceAdmissionFailureSite, SourceAdmissionFailure) {
        (self.site, *self.failure)
    }
}

fn classify_workspace_error(error: &WorkspaceError) -> SourceAdmissionErrorCategory {
    match error {
        WorkspaceError::Budget(_) => SourceAdmissionErrorCategory::Budget,
        WorkspaceError::Contract(_) => SourceAdmissionErrorCategory::Contract,
        WorkspaceError::Io { .. } => SourceAdmissionErrorCategory::Io,
        WorkspaceError::InvalidSource { .. } => SourceAdmissionErrorCategory::Content,
        WorkspaceError::UnsupportedSource { .. } => SourceAdmissionErrorCategory::Unsupported,
        WorkspaceError::InvalidSourceIdentity { .. }
        | WorkspaceError::InvalidSourceMemberIdentity { .. } => {
            SourceAdmissionErrorCategory::Identity
        }
        WorkspaceError::SourceChanged { .. } | WorkspaceError::ObservedSourceChanged { .. } => {
            SourceAdmissionErrorCategory::SourceChanged
        }
        WorkspaceError::SourceTooLarge { .. }
        | WorkspaceError::RangeOverflow { .. }
        | WorkspaceError::RangeOutOfBounds { .. } => SourceAdmissionErrorCategory::ResourceLimit,
        WorkspaceError::Allocation { .. } => SourceAdmissionErrorCategory::Allocation,
        WorkspaceError::Binary(error) | WorkspaceError::BinaryMember { source: error, .. } => {
            classify_binary_error(error)
        }
        WorkspaceError::Operation { source, .. } => {
            if let Some(error) = source.downcast_ref::<BudgetedYamlError>() {
                classify_yaml_error(error)
            } else if let Some(error) = source.downcast_ref::<ArchiveLoadError>() {
                classify_archive_error(error)
            } else if let Some(error) = source.downcast_ref::<PhysicalOriginError>() {
                classify_physical_origin_error(error)
            } else {
                SourceAdmissionErrorCategory::WorkspaceInvariant
            }
        }
        WorkspaceError::NotRootSource(_)
        | WorkspaceError::MissingSource(_)
        | WorkspaceError::MissingObject(_)
        | WorkspaceError::AmbiguousObject { .. }
        | WorkspaceError::PreparedArtifact(_)
        | WorkspaceError::PreparedArtifactSourceCompatibility(_) => {
            SourceAdmissionErrorCategory::WorkspaceInvariant
        }
    }
}

fn classify_binary_error(error: &BinaryError) -> SourceAdmissionErrorCategory {
    match error {
        BinaryError::Budget(_) => SourceAdmissionErrorCategory::Budget,
        BinaryError::Io(_) | BinaryError::Timeout(_) => SourceAdmissionErrorCategory::Io,
        BinaryError::MemoryError(_) | BinaryError::Allocation { .. } => {
            SourceAdmissionErrorCategory::Allocation
        }
        BinaryError::ResourceLimitExceeded(_) => SourceAdmissionErrorCategory::ResourceLimit,
        BinaryError::UnsupportedVersion(_)
        | BinaryError::UnsupportedCompression(_)
        | BinaryError::Unsupported(_) => SourceAdmissionErrorCategory::Unsupported,
        BinaryError::ObjectIdentity(_) => SourceAdmissionErrorCategory::Identity,
        BinaryError::InvalidFormat(_)
        | BinaryError::DecompressionFailed(_)
        | BinaryError::InvalidData(_)
        | BinaryError::ObjectReplacement(_)
        | BinaryError::ParseError(_)
        | BinaryError::NotEnoughData { .. }
        | BinaryError::InvalidSignature { .. }
        | BinaryError::CorruptedData(_)
        | BinaryError::Generic(_) => SourceAdmissionErrorCategory::Content,
    }
}

fn classify_yaml_error(error: &BudgetedYamlError) -> SourceAdmissionErrorCategory {
    match error {
        BudgetedYamlError::Io { .. } => SourceAdmissionErrorCategory::Io,
        BudgetedYamlError::SourceTooLarge { .. } | BudgetedYamlError::DepthExceeded { .. } => {
            SourceAdmissionErrorCategory::ResourceLimit
        }
        BudgetedYamlError::SourceChanged { .. } => SourceAdmissionErrorCategory::SourceChanged,
        BudgetedYamlError::Budget(_) => SourceAdmissionErrorCategory::Budget,
        BudgetedYamlError::AllocationFailed { .. }
        | BudgetedYamlError::IndexMapAllocationFailed { .. } => {
            SourceAdmissionErrorCategory::Allocation
        }
        BudgetedYamlError::AliasUnsupported { .. }
        | BudgetedYamlError::MergeKeyUnsupported { .. }
        | BudgetedYamlError::ComplexKeyUnsupported { .. } => {
            SourceAdmissionErrorCategory::Unsupported
        }
        BudgetedYamlError::InvalidUtf8 { .. }
        | BudgetedYamlError::Syntax(_)
        | BudgetedYamlError::InvalidHeader { .. }
        | BudgetedYamlError::UnexpectedAnchor { .. }
        | BudgetedYamlError::UnexpectedTag { .. }
        | BudgetedYamlError::DuplicateKey { .. }
        | BudgetedYamlError::InvalidDocument { .. } => SourceAdmissionErrorCategory::Content,
        _ => SourceAdmissionErrorCategory::Content,
    }
}

fn classify_archive_error(error: &ArchiveLoadError) -> SourceAdmissionErrorCategory {
    match error {
        ArchiveLoadError::Budget { .. } => SourceAdmissionErrorCategory::Budget,
        ArchiveLoadError::Allocation { .. } => SourceAdmissionErrorCategory::Allocation,
        ArchiveLoadError::ArithmeticOverflow { .. }
        | ArchiveLoadError::OccurrenceOverflow { .. } => {
            SourceAdmissionErrorCategory::ResourceLimit
        }
        ArchiveLoadError::InvalidMemberName { .. } | ArchiveLoadError::MemberIdentity { .. } => {
            SourceAdmissionErrorCategory::Identity
        }
        ArchiveLoadError::InvalidStructure { source }
        | ArchiveLoadError::ReadMember { source, .. } => classify_archive_io(source),
        ArchiveLoadError::OpenArchive { source } | ArchiveLoadError::OpenMember { source, .. } => {
            classify_zip_error(source)
        }
        ArchiveLoadError::InconsistentMetadata { .. } => SourceAdmissionErrorCategory::Content,
    }
}

fn classify_archive_io(error: &std::io::Error) -> SourceAdmissionErrorCategory {
    match error.kind() {
        std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof => {
            SourceAdmissionErrorCategory::Content
        }
        std::io::ErrorKind::OutOfMemory => SourceAdmissionErrorCategory::Allocation,
        _ => SourceAdmissionErrorCategory::Io,
    }
}

fn classify_zip_error(error: &zip::result::ZipError) -> SourceAdmissionErrorCategory {
    match error {
        zip::result::ZipError::Io(error) => classify_archive_io(error),
        zip::result::ZipError::UnsupportedArchive(_) => SourceAdmissionErrorCategory::Unsupported,
        zip::result::ZipError::InvalidArchive(_) | zip::result::ZipError::FileNotFound => {
            SourceAdmissionErrorCategory::Content
        }
    }
}

const fn classify_physical_origin_error(
    error: &PhysicalOriginError,
) -> SourceAdmissionErrorCategory {
    match error {
        PhysicalOriginError::Io { .. } => SourceAdmissionErrorCategory::Io,
        PhysicalOriginError::NotAbsolute(_) | PhysicalOriginError::NotRegularFile(_) => {
            SourceAdmissionErrorCategory::Contract
        }
        #[cfg(windows)]
        PhysicalOriginError::UnsupportedWindowsNamespace(_)
        | PhysicalOriginError::AlternateDataStream(_) => SourceAdmissionErrorCategory::Contract,
    }
}

impl AssetWorkspace {
    pub fn load_path(
        &mut self,
        path: impl Into<PathBuf>,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceError> {
        self.load_source(SourceOpenRequest::from_path(path)?, budget)
    }

    /// Loads one coherent filesystem image into a new immutable workspace revision.
    ///
    /// The opened root is scanned twice and its physical identity is revalidated before
    /// publication. Both passes and the retained shared backing are charged to `budget`.
    pub fn load_source(
        &mut self,
        request: SourceOpenRequest,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceError> {
        let report =
            self.admit_single_operation(SourceAdmissionOperation::LoadPath(request), budget)?;
        single_loaded_source(report)
    }

    /// Loads one caller-owned source image without reopening its physical path.
    ///
    /// The supplied bytes are the authoritative image retained by the resulting workspace
    /// revision. The path still establishes the durable physical origin used by publication and
    /// recovery checks, which will reject later on-disk divergence from this image's fingerprint.
    pub fn load_source_bytes(
        &mut self,
        request: SourceOpenRequest,
        image: Arc<[u8]>,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceError> {
        let report = self.admit_single_operation(
            SourceAdmissionOperation::LoadBytes { request, image },
            budget,
        )?;
        single_loaded_source(report)
    }

    /// Loads a source backing whose shared allocation has already been charged to `budget`.
    ///
    /// This ownership-transfer entry point prevents cooperating scanners from charging the same
    /// immutable allocation again when the workspace retains it. Proofs minted by another load
    /// budget are rejected before workspace allocations are charged.
    pub fn load_budgeted_source_bytes(
        &mut self,
        request: SourceOpenRequest,
        image: BudgetedSourceBytes,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceError> {
        image.validate_budget(budget)?;
        let report = self.admit_single_operation(
            SourceAdmissionOperation::LoadBudgetedBytes { request, image },
            budget,
        )?;
        single_loaded_source(report)
    }

    pub fn unload_source(
        &mut self,
        root: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), WorkspaceError> {
        let report = self.admit_single_operation(SourceAdmissionOperation::Unload(root), budget)?;
        single_unloaded_source(report, root)
    }

    fn admit_single_operation(
        &mut self,
        operation: SourceAdmissionOperation,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceAdmissionReport, WorkspaceError> {
        let batch = single_admission_batch(operation, budget)?;
        self.admit_sources(batch, SourceAdmissionPolicy::Strict, budget)
            .map_err(source_admission_error_to_workspace)
    }

    /// Prepares and applies an ordered source batch as one authoritative workspace transition.
    ///
    /// All load operations are fully parsed before the catalog/store candidate is created. A
    /// successful call installs at most one immutable state; any returned error leaves the
    /// authoritative state unchanged.
    pub fn admit_sources(
        &mut self,
        batch: SourceAdmissionBatch,
        policy: SourceAdmissionPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceAdmissionReport, SourceAdmissionError> {
        batch.validate_budget(budget).map_err(|error| {
            admission_batch_workspace_error(
                SourceAdmissionBatchPhase::Preparation,
                WorkspaceError::Budget(error),
            )
        })?;
        let base_revision = self.revision();
        let operations = batch.into_operations();
        if operations.is_empty() {
            return Ok(SourceAdmissionReport::new(
                policy,
                base_revision,
                base_revision,
                false,
                Vec::new(),
            ));
        }

        let mut load_count = 0;
        for (index, operation) in operations.iter().enumerate() {
            if !matches!(operation, SourceAdmissionOperation::Unload(_)) {
                load_count += 1;
            }
            if let SourceAdmissionOperation::LoadBudgetedBytes { image, .. } = operation {
                let ordinal = admission_ordinal(index).map_err(|error| {
                    admission_batch_workspace_error(SourceAdmissionBatchPhase::Preparation, error)
                })?;
                image.validate_budget(budget).map_err(|error| {
                    SourceAdmissionError::operation(
                        ordinal,
                        None,
                        SourceAdmissionFailure::from(WorkspaceError::from(error)),
                    )
                })?;
            }
        }
        let indexes = (load_count > 1)
            .then(|| AdmissionConflictIndexes::reserve(load_count, budget))
            .transpose()
            .map_err(|error| {
                admission_batch_workspace_error(SourceAdmissionBatchPhase::Preparation, error)
            })?;
        let mut prepared = reserve_budgeted_vec::<PreparedAdmission>(
            operations.len(),
            budget,
            "source admission preparation",
        )
        .map_err(|error| {
            admission_batch_workspace_error(SourceAdmissionBatchPhase::Preparation, error)
        })?;

        let mut has_action = false;
        for (index, operation) in operations.into_iter().enumerate() {
            let ordinal = admission_ordinal(index).map_err(|error| {
                admission_batch_workspace_error(SourceAdmissionBatchPhase::Preparation, error)
            })?;
            let result = match operation {
                SourceAdmissionOperation::LoadPath(request) => {
                    self.prepare_admission_load(ordinal, request, SourceImageInput::Path, budget)
                }
                SourceAdmissionOperation::LoadBytes { request, image } => self
                    .prepare_admission_load(
                        ordinal,
                        request,
                        SourceImageInput::Unaccounted(image),
                        budget,
                    ),
                SourceAdmissionOperation::LoadBudgetedBytes { request, image } => self
                    .prepare_admission_load(
                        ordinal,
                        request,
                        SourceImageInput::Budgeted(image),
                        budget,
                    ),
                SourceAdmissionOperation::Unload(source) => {
                    Ok(PreparedAdmission::Unload { ordinal, source })
                }
            };
            match result {
                Ok(operation) => {
                    has_action |= operation.is_action();
                    prepared.push(operation);
                }
                Err(failure) if policy.tolerates(failure.category()) => {
                    prepared.push(PreparedAdmission::Rejected {
                        ordinal,
                        rejection: SourceAdmissionRejection::new(
                            failure.location,
                            *failure.failure,
                        ),
                    });
                }
                Err(failure) => {
                    return Err(SourceAdmissionError::operation(
                        ordinal,
                        failure.location,
                        *failure.failure,
                    ));
                }
            }
        }

        self.apply_prepared_admissions(prepared, has_action, indexes, policy, base_revision, budget)
    }

    fn prepare_admission_load(
        &self,
        ordinal: u64,
        request: SourceOpenRequest,
        image: SourceImageInput,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedAdmission, AdmissionOperationFailure> {
        let SourceOpenRequest {
            path,
            alias,
            kind_hint,
            companions,
        } = request;
        if !companions.is_empty() && !matches!(&image, SourceImageInput::Path) {
            return Err(AdmissionOperationFailure::with_location(
                SourceAdmissionOperationLocation::RequestPath(path),
                SourceAdmissionFailure::CompanionsRequireFilesystemLoad,
            ));
        }
        let absolute = match absolute_path(path, budget) {
            Ok(absolute) => absolute,
            Err((path, error)) => {
                return Err(AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::RequestPath(path),
                    SourceAdmissionFailure::from(*error),
                ));
            }
        };
        let origin = match PhysicalOrigin::from_existing_path_budgeted(&absolute, budget) {
            Ok(origin) => origin,
            Err(error) => {
                return Err(AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::RequestPath(absolute),
                    SourceAdmissionFailure::from(physical_origin_workspace_error(error)),
                ));
            }
        };
        drop(absolute);
        let image = match image {
            SourceImageInput::Path => match read_owned_image(&origin, budget) {
                Ok(image) => image,
                Err(error) => {
                    return Err(AdmissionOperationFailure::with_location(
                        SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                        SourceAdmissionFailure::from(error),
                    ));
                }
            },
            SourceImageInput::Unaccounted(image) => {
                match BudgetedSourceBytes::from_arc(image, budget).map_err(WorkspaceError::from) {
                    Ok(image) => image,
                    Err(error) => {
                        return Err(AdmissionOperationFailure::with_location(
                            SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                            SourceAdmissionFailure::from(error),
                        ));
                    }
                }
            }
            SourceImageInput::Budgeted(image) => {
                if let Err(error) = image.validate_budget(budget).map_err(WorkspaceError::from) {
                    return Err(AdmissionOperationFailure::with_location(
                        SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                        SourceAdmissionFailure::from(error),
                    ));
                }
                image
            }
        };
        let mut source = match prepare_root(
            origin.path(),
            kind_hint,
            image,
            self.binary_adapter(),
            self.source_registry(),
            budget,
        ) {
            Ok(source) => source,
            Err(error) => {
                return Err(AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                    SourceAdmissionFailure::from(error),
                ));
            }
        };
        if !companions.is_empty() {
            if !matches!(source.kind(), SourceKind::Yaml | SourceKind::SerializedFile) {
                return Err(AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                    SourceAdmissionFailure::InvalidCompanionParentKind {
                        actual: source.kind(),
                    },
                ));
            }
            let existing_root = match self.state().catalog().root_admission_decision(
                &alias,
                &origin,
                source.fingerprint(),
            ) {
                Ok(RootAdmissionDecision::Unchanged(existing)) => Some(existing),
                Ok(
                    RootAdmissionDecision::Vacant
                    | RootAdmissionDecision::AliasConflict { .. }
                    | RootAdmissionDecision::PhysicalOriginConflict { .. },
                ) => None,
                Err(error) => {
                    return Err(AdmissionOperationFailure::with_location(
                        SourceAdmissionOperationLocation::PhysicalOrigin(
                            origin.path().to_path_buf(),
                        ),
                        SourceAdmissionFailure::from(WorkspaceError::from(error)),
                    ));
                }
            };
            let prepared =
                prepare_source_companions(self, &origin, existing_root, companions, budget)?;
            source.attach_companions(prepared);
        }
        Ok(PreparedAdmission::Load {
            ordinal,
            alias,
            origin,
            source: Box::new(source),
        })
    }

    fn apply_prepared_admissions(
        &mut self,
        prepared: Vec<PreparedAdmission>,
        has_action: bool,
        mut indexes: Option<AdmissionConflictIndexes>,
        policy: SourceAdmissionPolicy,
        base_revision: WorkspaceRevision,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceAdmissionReport, SourceAdmissionError> {
        let mut outcomes = reserve_budgeted_vec::<SourceAdmissionOutcome>(
            prepared.len(),
            budget,
            "source admission outcomes",
        )
        .map_err(|error| {
            admission_batch_workspace_error(SourceAdmissionBatchPhase::CandidateApplication, error)
        })?;

        if !has_action {
            for operation in prepared {
                let PreparedAdmission::Rejected { ordinal, rejection } = operation else {
                    return Err(admission_batch_protocol_error(
                        SourceAdmissionBatchPhase::CandidateApplication,
                        SourceAdmissionProtocolError::MissingAction,
                    ));
                };
                outcomes.push(SourceAdmissionOutcome::new(
                    ordinal,
                    SourceAdmissionDisposition::Rejected(rejection),
                ));
            }
            return Ok(SourceAdmissionReport::new(
                policy,
                base_revision,
                base_revision,
                false,
                outcomes,
            ));
        }
        let mut transaction = WorkspaceStateTransaction::begin(Arc::clone(self.state()), budget)
            .map_err(WorkspaceError::from)
            .map_err(|error| {
                admission_batch_workspace_error(
                    SourceAdmissionBatchPhase::CandidateApplication,
                    error,
                )
            })?;
        let mut candidate_changed = false;

        for operation in prepared {
            match operation {
                PreparedAdmission::Rejected { ordinal, rejection } => {
                    outcomes.push(SourceAdmissionOutcome::new(
                        ordinal,
                        SourceAdmissionDisposition::Rejected(rejection),
                    ));
                }
                PreparedAdmission::Unload { ordinal, source } => {
                    if source.workspace() != self.workspace_id() {
                        let error = unity_asset_core::ContractError::WorkspaceMismatch {
                            expected: self.workspace_id(),
                            actual: source.workspace(),
                        };
                        return Err(admission_workspace_error_at(
                            ordinal,
                            SourceAdmissionOperationLocation::Source(source),
                            error.into(),
                        ));
                    }
                    let is_root = transaction
                        .is_root(source)
                        .map_err(WorkspaceError::from)
                        .map_err(|error| {
                            admission_workspace_error_at(
                                ordinal,
                                SourceAdmissionOperationLocation::Source(source),
                                error,
                            )
                        })?;
                    if !is_root {
                        return Err(admission_workspace_error_at(
                            ordinal,
                            SourceAdmissionOperationLocation::Source(source),
                            WorkspaceError::NotRootSource(source),
                        ));
                    }
                    transaction
                        .remove_subtree(source, budget)
                        .map_err(WorkspaceError::from)
                        .map_err(|error| {
                            admission_workspace_error_at(
                                ordinal,
                                SourceAdmissionOperationLocation::Source(source),
                                error,
                            )
                        })?;
                    if let Some(indexes) = &mut indexes {
                        indexes.remove(source);
                    }
                    candidate_changed = true;
                    outcomes.push(SourceAdmissionOutcome::new(
                        ordinal,
                        SourceAdmissionDisposition::Unloaded { source_id: source },
                    ));
                }
                PreparedAdmission::Load {
                    ordinal,
                    alias,
                    origin,
                    source,
                } => {
                    if let Some((first_operation, _)) = indexes
                        .as_ref()
                        .and_then(|indexes| indexes.aliases.get(&alias))
                    {
                        retain_admission_conflict(
                            policy,
                            ordinal,
                            SourceAdmissionOperationLocation::Alias(alias),
                            SourceAdmissionFailure::DuplicateAlias {
                                first_operation: *first_operation,
                            },
                            &mut outcomes,
                        )?;
                        continue;
                    }
                    if let Some((first_operation, _)) = indexes
                        .as_ref()
                        .and_then(|indexes| indexes.origins.get(&origin))
                    {
                        retain_admission_conflict(
                            policy,
                            ordinal,
                            SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                            SourceAdmissionFailure::DuplicatePhysicalOrigin {
                                first_operation: *first_operation,
                            },
                            &mut outcomes,
                        )?;
                        continue;
                    }
                    let fingerprint = source.fingerprint();
                    let decision = transaction
                        .root_admission_decision(&alias, &origin, fingerprint)
                        .map_err(WorkspaceError::from)
                        .map_err(|error| admission_workspace_error(ordinal, error))?;
                    let existing = match decision {
                        RootAdmissionDecision::Vacant => None,
                        RootAdmissionDecision::Unchanged(existing) => Some(existing),
                        RootAdmissionDecision::AliasConflict { existing } => {
                            retain_admission_conflict(
                                policy,
                                ordinal,
                                SourceAdmissionOperationLocation::Alias(alias),
                                SourceAdmissionFailure::AliasConflict {
                                    existing_source: existing,
                                },
                                &mut outcomes,
                            )?;
                            continue;
                        }
                        RootAdmissionDecision::PhysicalOriginConflict { existing } => {
                            retain_admission_conflict(
                                policy,
                                ordinal,
                                SourceAdmissionOperationLocation::PhysicalOrigin(
                                    origin.into_path(),
                                ),
                                SourceAdmissionFailure::PhysicalOriginConflict {
                                    existing_source: existing,
                                },
                                &mut outcomes,
                            )?;
                            continue;
                        }
                    };

                    let insertion = match &mut indexes {
                        Some(indexes) => match indexes.prepare_insertion(&alias, &origin, budget) {
                            Ok(insertion) => Some(insertion),
                            Err(AdmissionIndexKeyError::Alias(error)) => {
                                return Err(SourceAdmissionError::operation(
                                    ordinal,
                                    Some(SourceAdmissionOperationLocation::Alias(alias)),
                                    SourceAdmissionFailure::from(*error),
                                ));
                            }
                            Err(AdmissionIndexKeyError::Origin(error)) => {
                                return Err(SourceAdmissionError::operation(
                                    ordinal,
                                    Some(SourceAdmissionOperationLocation::PhysicalOrigin(
                                        origin.into_path(),
                                    )),
                                    SourceAdmissionFailure::from(*error),
                                ));
                            }
                        },
                        None => None,
                    };
                    if let Some(existing) = existing
                        && !source.has_children()
                    {
                        if let Some(insertion) = insertion {
                            insertion.insert(ordinal, existing);
                        }
                        outcomes.push(SourceAdmissionOutcome::new(
                            ordinal,
                            SourceAdmissionDisposition::Unchanged {
                                source_id: existing,
                            },
                        ));
                        continue;
                    }
                    let root_descriptor = SourceDescriptor::root(source.kind(), alias, origin);
                    let registration = transaction
                        .reconcile_tree(root_descriptor, *source, budget)
                        .map_err(WorkspaceError::from)
                        .map_err(|error| admission_workspace_error(ordinal, error))?;
                    let root = registration.source();
                    debug_assert!(existing.is_none_or(|existing| existing == root));
                    if let Some(insertion) = insertion {
                        insertion.insert(ordinal, root);
                    }
                    let changed = registration.changed();
                    candidate_changed |= changed;
                    outcomes.push(SourceAdmissionOutcome::new(
                        ordinal,
                        if changed {
                            SourceAdmissionDisposition::Loaded { source_id: root }
                        } else {
                            SourceAdmissionDisposition::Unchanged { source_id: root }
                        },
                    ));
                }
            }
        }

        if !candidate_changed {
            return Ok(SourceAdmissionReport::new(
                policy,
                base_revision,
                base_revision,
                false,
                outcomes,
            ));
        }

        let prepared_state = transaction
            .commit(budget)
            .map_err(WorkspaceError::from)
            .map_err(|error| {
                admission_batch_workspace_error(SourceAdmissionBatchPhase::Publication, error)
            })?;
        let revision = prepared_state.revision();
        let state_installed = match self.install_prepared_state(&prepared_state) {
            WorkspaceStateInstallOutcome::Installed => true,
            WorkspaceStateInstallOutcome::Unchanged => false,
            WorkspaceStateInstallOutcome::Stale => {
                return Err(admission_batch_workspace_error(
                    SourceAdmissionBatchPhase::Publication,
                    WorkspaceError::operation(
                        "source admission state installation",
                        std::io::Error::other(
                            "workspace changed before the prepared state could be installed",
                        ),
                    ),
                ));
            }
        };
        Ok(SourceAdmissionReport::new(
            policy,
            base_revision,
            revision,
            state_installed,
            outcomes,
        ))
    }
}

#[derive(Debug)]
struct AdmissionOperationFailure {
    location: Option<SourceAdmissionOperationLocation>,
    failure: Box<SourceAdmissionFailure>,
}

impl AdmissionOperationFailure {
    fn with_location(
        location: SourceAdmissionOperationLocation,
        failure: SourceAdmissionFailure,
    ) -> Self {
        Self {
            location: Some(location),
            failure: Box::new(failure),
        }
    }

    #[must_use]
    fn category(&self) -> SourceAdmissionErrorCategory {
        self.failure.category()
    }
}

#[derive(Debug)]
enum AdmissionIndexKeyError {
    Alias(Box<WorkspaceError>),
    Origin(Box<WorkspaceError>),
}

#[derive(Debug)]
struct AdmissionConflictIndexes {
    aliases: HashMap<SourceAlias, (u64, SourceId)>,
    origins: HashMap<PhysicalOrigin, (u64, SourceId)>,
}

impl AdmissionConflictIndexes {
    fn reserve(count: usize, budget: &mut AssetLoadBudget) -> Result<Self, WorkspaceError> {
        Ok(Self {
            aliases: reserve_admission_index(count, budget, "source admission alias index")?,
            origins: reserve_admission_index(
                count,
                budget,
                "source admission physical-origin index",
            )?,
        })
    }

    fn prepare_insertion<'a>(
        &'a mut self,
        alias: &SourceAlias,
        origin: &PhysicalOrigin,
        budget: &mut AssetLoadBudget,
    ) -> Result<AdmissionConflictInsertion<'a>, AdmissionIndexKeyError> {
        let alias = clone_admission_alias(alias, budget, "source admission alias key")
            .map_err(|error| AdmissionIndexKeyError::Alias(Box::new(error)))?;
        let origin = clone_admission_origin(origin, budget, "source admission physical-origin key")
            .map_err(|error| AdmissionIndexKeyError::Origin(Box::new(error)))?;
        Ok(AdmissionConflictInsertion {
            indexes: self,
            alias,
            origin,
        })
    }

    fn remove(&mut self, source: SourceId) {
        self.aliases
            .retain(|_, (_, indexed_source)| *indexed_source != source);
        self.origins
            .retain(|_, (_, indexed_source)| *indexed_source != source);
    }
}

struct AdmissionConflictInsertion<'a> {
    indexes: &'a mut AdmissionConflictIndexes,
    alias: SourceAlias,
    origin: PhysicalOrigin,
}

impl AdmissionConflictInsertion<'_> {
    fn insert(self, ordinal: u64, source: SourceId) {
        self.indexes.aliases.insert(self.alias, (ordinal, source));
        self.indexes.origins.insert(self.origin, (ordinal, source));
    }
}

#[derive(Debug)]
enum PreparedAdmission {
    Load {
        ordinal: u64,
        alias: SourceAlias,
        origin: PhysicalOrigin,
        source: Box<PreparedSourceTree>,
    },
    Unload {
        ordinal: u64,
        source: SourceId,
    },
    Rejected {
        ordinal: u64,
        rejection: SourceAdmissionRejection,
    },
}

impl PreparedAdmission {
    const fn is_action(&self) -> bool {
        matches!(self, Self::Load { .. } | Self::Unload { .. })
    }
}

#[derive(Debug, thiserror::Error)]
enum SourceAdmissionProtocolError {
    #[error("a prepared batch without rejected-only operations has no action")]
    MissingAction,
    #[error("a single-operation admission returned {actual} outcomes")]
    UnexpectedOutcomeCount { actual: usize },
    #[error("a single load returned a non-load disposition")]
    UnexpectedLoadDisposition,
    #[error("a single unload returned a non-unload disposition")]
    UnexpectedUnloadDisposition,
}

fn single_admission_batch(
    operation: SourceAdmissionOperation,
    budget: &mut AssetLoadBudget,
) -> Result<SourceAdmissionBatch, WorkspaceError> {
    let mut batch = SourceAdmissionBatch::with_capacity(1, budget)
        .map_err(admission_batch_allocation_to_workspace)?;
    batch
        .try_push(operation, budget)
        .map_err(admission_batch_push_to_workspace)?;
    Ok(batch)
}

fn admission_batch_allocation_to_workspace(
    error: SourceAdmissionBatchAllocationError,
) -> WorkspaceError {
    match error {
        SourceAdmissionBatchAllocationError::Budget(error) => WorkspaceError::Budget(error),
        error @ SourceAdmissionBatchAllocationError::Allocation { .. } => {
            WorkspaceError::operation("source admission batch allocation", error)
        }
    }
}

fn admission_batch_push_to_workspace(error: SourceAdmissionBatchPushError) -> WorkspaceError {
    match error {
        SourceAdmissionBatchPushError::Budget(error) => WorkspaceError::Budget(error),
        SourceAdmissionBatchPushError::Capacity(error) => {
            WorkspaceError::operation("single source admission batch", error)
        }
    }
}

fn single_loaded_source(report: SourceAdmissionReport) -> Result<SourceId, WorkspaceError> {
    match take_single_disposition(report)? {
        SourceAdmissionDisposition::Loaded { source_id }
        | SourceAdmissionDisposition::Unchanged { source_id } => Ok(source_id),
        SourceAdmissionDisposition::Rejected(rejection) => {
            Err(admission_rejection_to_workspace(rejection))
        }
        SourceAdmissionDisposition::Unloaded { .. } => Err(WorkspaceError::operation(
            "single source admission",
            SourceAdmissionProtocolError::UnexpectedLoadDisposition,
        )),
    }
}

fn single_unloaded_source(
    report: SourceAdmissionReport,
    expected: SourceId,
) -> Result<(), WorkspaceError> {
    match take_single_disposition(report)? {
        SourceAdmissionDisposition::Unloaded { source_id } if source_id == expected => Ok(()),
        SourceAdmissionDisposition::Rejected(rejection) => {
            Err(admission_rejection_to_workspace(rejection))
        }
        SourceAdmissionDisposition::Loaded { .. }
        | SourceAdmissionDisposition::Unchanged { .. }
        | SourceAdmissionDisposition::Unloaded { .. } => Err(WorkspaceError::operation(
            "single source admission",
            SourceAdmissionProtocolError::UnexpectedUnloadDisposition,
        )),
    }
}

fn take_single_disposition(
    report: SourceAdmissionReport,
) -> Result<SourceAdmissionDisposition, WorkspaceError> {
    let actual = report.outcomes().len();
    if actual != 1 {
        return Err(WorkspaceError::operation(
            "single source admission",
            SourceAdmissionProtocolError::UnexpectedOutcomeCount { actual },
        ));
    }
    let mut outcomes = report.into_outcomes().into_iter();
    let Some(outcome) = outcomes.next() else {
        return Err(WorkspaceError::operation(
            "single source admission",
            SourceAdmissionProtocolError::UnexpectedOutcomeCount { actual: 0 },
        ));
    };
    Ok(outcome.into_disposition())
}

fn source_admission_error_to_workspace(error: SourceAdmissionError) -> WorkspaceError {
    let (site, failure) = error.into_parts();
    located_admission_failure_to_workspace(site.into_operation_location(), failure)
}

fn admission_rejection_to_workspace(rejection: SourceAdmissionRejection) -> WorkspaceError {
    let (location, failure) = rejection.into_parts();
    located_admission_failure_to_workspace(location, failure)
}

fn located_admission_failure_to_workspace(
    location: Option<SourceAdmissionOperationLocation>,
    failure: SourceAdmissionFailure,
) -> WorkspaceError {
    match failure {
        SourceAdmissionFailure::Workspace(error) => *error,
        failure @ SourceAdmissionFailure::CompanionsRequireFilesystemLoad
        | failure @ SourceAdmissionFailure::InvalidCompanionParentKind { .. }
        | failure @ SourceAdmissionFailure::InvalidCompanionOccurrence { .. }
        | failure @ SourceAdmissionFailure::CompanionPathMismatch { .. }
        | failure @ SourceAdmissionFailure::DuplicateAlias { .. }
        | failure @ SourceAdmissionFailure::DuplicatePhysicalOrigin { .. }
        | failure @ SourceAdmissionFailure::AliasConflict { .. }
        | failure @ SourceAdmissionFailure::PhysicalOriginConflict { .. } => match location {
            Some(SourceAdmissionOperationLocation::Alias(alias)) => WorkspaceError::InvalidSource {
                path: PathBuf::from(alias.as_str()),
                message: failure.to_string(),
            },
            Some(SourceAdmissionOperationLocation::RequestPath(path))
            | Some(SourceAdmissionOperationLocation::PhysicalOrigin(path)) => {
                WorkspaceError::InvalidSource {
                    path,
                    message: failure.to_string(),
                }
            }
            Some(SourceAdmissionOperationLocation::Source(_)) | None => {
                WorkspaceError::operation("source admission", failure)
            }
        },
    }
}

fn prepare_source_companions(
    workspace: &AssetWorkspace,
    root: &PhysicalOrigin,
    existing_root: Option<SourceId>,
    companions: Vec<SourceCompanionRequest>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<PreparedSourceChild>, AdmissionOperationFailure> {
    let mut prepared = reserve_budgeted_vec::<PreparedSourceChild>(
        companions.len(),
        budget,
        "source admission companions",
    )
    .map_err(|error| AdmissionOperationFailure {
        location: None,
        failure: Box::new(SourceAdmissionFailure::from(error)),
    })?;

    for companion in companions {
        let SourceCompanionRequest { path, member } = companion;
        let same_name_occurrence = member.same_name_occurrence();
        if same_name_occurrence != 0 {
            return Err(AdmissionOperationFailure::with_location(
                SourceAdmissionOperationLocation::RequestPath(path),
                SourceAdmissionFailure::InvalidCompanionOccurrence {
                    member,
                    same_name_occurrence,
                },
            ));
        }

        let absolute = match absolute_path(path, budget) {
            Ok(absolute) => absolute,
            Err((path, error)) => {
                return Err(AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::RequestPath(path),
                    SourceAdmissionFailure::from(*error),
                ));
            }
        };
        let origin = match PhysicalOrigin::from_existing_path_budgeted(&absolute, budget) {
            Ok(origin) => origin,
            Err(error) => {
                return Err(AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::RequestPath(absolute),
                    SourceAdmissionFailure::from(physical_origin_workspace_error(error)),
                ));
            }
        };
        drop(absolute);

        let expected_path = budgeted_companion_path(root, &member, budget).map_err(|error| {
            AdmissionOperationFailure::with_location(
                SourceAdmissionOperationLocation::PhysicalOrigin(root.path().to_path_buf()),
                SourceAdmissionFailure::from(error),
            )
        })?;
        let expected = match PhysicalOrigin::from_existing_path_budgeted(&expected_path, budget) {
            Ok(expected) => expected,
            Err(error) => {
                return Err(AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::RequestPath(expected_path),
                    SourceAdmissionFailure::from(physical_origin_workspace_error(error)),
                ));
            }
        };
        if origin != expected {
            return Err(AdmissionOperationFailure::with_location(
                SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                SourceAdmissionFailure::CompanionPathMismatch {
                    expected: expected.into_path(),
                },
            ));
        }

        let existing_companion = match existing_root {
            Some(parent) => workspace
                .state()
                .catalog()
                .child_by_member(parent, ContainmentKind::Companion, &member)
                .map_err(|error| {
                    AdmissionOperationFailure::with_location(
                        SourceAdmissionOperationLocation::PhysicalOrigin(
                            origin.path().to_path_buf(),
                        ),
                        SourceAdmissionFailure::from(WorkspaceError::from(error)),
                    )
                })?,
            None => None,
        };
        if let Some(existing) = existing_companion {
            let catalog = workspace.state().catalog();
            let registered_origin = catalog.physical_origin(existing).map_err(|error| {
                AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::PhysicalOrigin(origin.path().to_path_buf()),
                    SourceAdmissionFailure::from(WorkspaceError::from(error)),
                )
            })?;
            if registered_origin != &origin {
                return Err(AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                    SourceAdmissionFailure::PhysicalOriginConflict {
                        existing_source: existing,
                    },
                ));
            }
            let expected_fingerprint = catalog.fingerprint(existing).map_err(|error| {
                AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::PhysicalOrigin(origin.path().to_path_buf()),
                    SourceAdmissionFailure::from(WorkspaceError::from(error)),
                )
            })?;
            if let Err(error) = VerifiedPhysicalBinding::verify_existing(
                SourceKind::StreamedResource,
                origin.path(),
                expected_fingerprint,
                budget,
            ) {
                let error = companion_verification_workspace_error(origin.path(), error);
                return Err(AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                    SourceAdmissionFailure::from(error),
                ));
            }
            continue;
        }

        let image = read_owned_image(&origin, budget).map_err(|error| {
            AdmissionOperationFailure::with_location(
                SourceAdmissionOperationLocation::PhysicalOrigin(origin.path().to_path_buf()),
                SourceAdmissionFailure::from(error),
            )
        })?;
        prepared.push(PreparedSourceChild::new(
            PreparedSourceRelation::Companion(origin),
            member,
            prepared_raw(image),
        ));
    }
    Ok(prepared)
}

fn budgeted_companion_path(
    root: &PhysicalOrigin,
    member: &SourceMemberId,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, WorkspaceError> {
    const RESOURCE: &str = "source admission companion path";

    let parent = root
        .path()
        .parent()
        .ok_or_else(|| WorkspaceError::InvalidSource {
            path: root.path().to_path_buf(),
            message: "the root physical origin has no parent directory".to_owned(),
        })?;
    let tail = Path::new(member.name());
    let requested = parent
        .as_os_str()
        .len()
        .checked_add(tail.as_os_str().len())
        .and_then(|bytes| bytes.checked_add(1))
        .ok_or(BudgetError::ArithmeticOverflow { resource: RESOURCE })?;
    budget.check_bytes(usize_to_u64(requested, RESOURCE)?)?;
    let mut path = PathBuf::new();
    path.as_mut_os_string()
        .try_reserve_exact(requested)
        .map_err(|error| WorkspaceError::Allocation {
            resource: RESOURCE,
            requested,
            unit: WorkspaceAllocationUnit::Bytes,
            message: error.to_string(),
        })?;
    path.push(parent);
    path.push(tail);
    budget.consume_bytes(usize_to_u64(path.capacity(), RESOURCE)?)?;
    Ok(path)
}

fn admission_workspace_error(ordinal: u64, error: WorkspaceError) -> SourceAdmissionError {
    SourceAdmissionError::operation(ordinal, None, SourceAdmissionFailure::from(error))
}

fn retain_admission_conflict(
    policy: SourceAdmissionPolicy,
    ordinal: u64,
    location: SourceAdmissionOperationLocation,
    failure: SourceAdmissionFailure,
    outcomes: &mut Vec<SourceAdmissionOutcome>,
) -> Result<(), SourceAdmissionError> {
    if !policy.tolerates(failure.category()) {
        return Err(SourceAdmissionError::operation(
            ordinal,
            Some(location),
            failure,
        ));
    }
    outcomes.push(SourceAdmissionOutcome::new(
        ordinal,
        SourceAdmissionDisposition::Rejected(SourceAdmissionRejection::new(
            Some(location),
            failure,
        )),
    ));
    Ok(())
}

fn admission_workspace_error_at(
    ordinal: u64,
    location: SourceAdmissionOperationLocation,
    error: WorkspaceError,
) -> SourceAdmissionError {
    SourceAdmissionError::operation(ordinal, Some(location), SourceAdmissionFailure::from(error))
}

fn admission_batch_workspace_error(
    phase: SourceAdmissionBatchPhase,
    error: WorkspaceError,
) -> SourceAdmissionError {
    SourceAdmissionError::batch(phase, SourceAdmissionFailure::from(error))
}

fn admission_batch_protocol_error(
    phase: SourceAdmissionBatchPhase,
    error: SourceAdmissionProtocolError,
) -> SourceAdmissionError {
    admission_batch_workspace_error(
        phase,
        WorkspaceError::operation("source admission protocol", error),
    )
}

fn physical_origin_workspace_error(error: CatalogError) -> WorkspaceError {
    match error {
        CatalogError::InvalidPhysicalOrigin(error) => {
            WorkspaceError::operation("physical-origin validation", error)
        }
        error => WorkspaceError::from(error),
    }
}

fn companion_verification_workspace_error(path: &Path, error: CatalogError) -> WorkspaceError {
    match error {
        CatalogError::VerifiedPhysicalBindingIo {
            path,
            kind,
            message,
        } => WorkspaceError::Io {
            path,
            kind,
            message,
        },
        CatalogError::VerifiedPhysicalBindingChanged { path } => {
            WorkspaceError::SourceChanged { path }
        }
        CatalogError::VerifiedFingerprintMismatch { .. } => WorkspaceError::SourceChanged {
            path: path.to_path_buf(),
        },
        error => WorkspaceError::from(error),
    }
}

fn admission_ordinal(index: usize) -> Result<u64, WorkspaceError> {
    u64::try_from(index).map_err(|_| {
        WorkspaceError::Budget(BudgetError::ArithmeticOverflow {
            resource: "source_admission_ordinal",
        })
    })
}

fn reserve_admission_index<K, V>(
    count: usize,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<HashMap<K, V>, WorkspaceError>
where
    K: Eq + std::hash::Hash,
{
    // Cover capacity rounding, one control byte per bucket, and the trailing control group.
    let bucket_bytes = size_of::<(K, V)>()
        .checked_add(size_of::<u8>())
        .and_then(|bytes| bytes.checked_mul(count))
        .and_then(|bytes| bytes.checked_mul(2))
        .and_then(|bytes| bytes.checked_add(64))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bucket_bytes)?;
    let mut index = HashMap::new();
    index
        .try_reserve(count)
        .map_err(|error| WorkspaceError::Allocation {
            resource,
            requested: count,
            unit: WorkspaceAllocationUnit::Slots,
            message: error.to_string(),
        })?;
    budget.consume_bytes(bucket_bytes)?;
    Ok(index)
}

fn clone_admission_alias(
    alias: &SourceAlias,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<SourceAlias, WorkspaceError> {
    let length = alias.retained_clone_bytes();
    let bytes = u64::try_from(length).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let mut value = String::new();
    value
        .try_reserve_exact(length)
        .map_err(|error| WorkspaceError::Allocation {
            resource,
            requested: length,
            unit: WorkspaceAllocationUnit::Bytes,
            message: error.to_string(),
        })?;
    value.push_str(alias.as_str());
    let cloned = SourceAlias::new(value).map_err(WorkspaceError::from)?;
    let retained_bytes = u64::try_from(cloned.retained_owned_bytes())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(cloned)
}

fn clone_admission_origin(
    origin: &PhysicalOrigin,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<PhysicalOrigin, WorkspaceError> {
    let length = origin.path().as_os_str().len();
    let bytes = u64::try_from(length).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let cloned = origin
        .try_clone_for_index()
        .map_err(|error| WorkspaceError::Allocation {
            resource,
            requested: length,
            unit: WorkspaceAllocationUnit::Bytes,
            message: error.to_string(),
        })?;
    let retained_bytes = u64::try_from(cloned.retained_owned_bytes())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(cloned)
}

fn absolute_path(
    path: PathBuf,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, (PathBuf, Box<WorkspaceError>)> {
    const RESOURCE: &str = "source admission absolute path";

    if path.is_absolute() {
        return Ok(path);
    }
    let additional = match path.as_os_str().len().checked_add(1) {
        Some(additional) => additional,
        None => {
            return Err((
                path,
                Box::new(BudgetError::ArithmeticOverflow { resource: RESOURCE }.into()),
            ));
        }
    };
    let additional_bytes = match usize_to_u64(additional, RESOURCE) {
        Ok(additional_bytes) => additional_bytes,
        Err(error) => return Err((path, Box::new(error))),
    };
    if let Err(error) = budget.check_bytes(additional_bytes) {
        return Err((path, Box::new(error.into())));
    }
    let mut directory = match std::env::current_dir() {
        Ok(directory) => directory,
        Err(error) => {
            let error = WorkspaceError::io(&path, error);
            return Err((path, Box::new(error)));
        }
    };
    let minimum_capacity = match directory.as_os_str().len().checked_add(additional) {
        Some(minimum_capacity) => minimum_capacity.max(directory.capacity()),
        None => {
            return Err((
                path,
                Box::new(BudgetError::ArithmeticOverflow { resource: RESOURCE }.into()),
            ));
        }
    };
    let minimum_bytes = match usize_to_u64(minimum_capacity, RESOURCE) {
        Ok(minimum_bytes) => minimum_bytes,
        Err(error) => return Err((path, Box::new(error))),
    };
    if let Err(error) = budget.check_bytes(minimum_bytes) {
        return Err((path, Box::new(error.into())));
    }
    if let Err(error) = directory.as_mut_os_string().try_reserve_exact(additional) {
        return Err((
            path,
            Box::new(WorkspaceError::Allocation {
                resource: RESOURCE,
                requested: additional,
                unit: WorkspaceAllocationUnit::Bytes,
                message: error.to_string(),
            }),
        ));
    }
    let retained_bytes = match usize_to_u64(directory.capacity(), RESOURCE) {
        Ok(retained_bytes) => retained_bytes,
        Err(error) => return Err((path, Box::new(error))),
    };
    if let Err(error) = budget.check_bytes(retained_bytes) {
        return Err((path, Box::new(error.into())));
    }
    directory.push(&path);
    if let Err(error) = budget.consume_bytes(retained_bytes) {
        return Err((path, Box::new(error.into())));
    }
    Ok(directory)
}

#[cfg(test)]
mod workspace_load_tests {
    use std::fs;

    use super::*;
    use unity_asset_core::AssetLoadLimits;

    #[test]
    fn absolute_request_path_transfers_without_a_second_budgeted_allocation() {
        let path = std::env::current_dir().expect("current directory");
        let capacity = path.capacity();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        })
        .expect("valid one-byte budget");

        let absolute = absolute_path(path, &mut budget).expect("transfer absolute path");

        assert!(absolute.is_absolute());
        assert_eq!(absolute.capacity(), capacity);
        assert_eq!(budget.usage(), Default::default());
    }

    #[test]
    fn relative_request_path_materialization_is_exact_budgeted() {
        let request = || PathBuf::from("relative").join("source.resource");
        let mut measured = AssetLoadBudget::default();
        let absolute =
            absolute_path(request(), &mut measured).expect("materialize measured absolute path");
        let measured_usage = measured.usage();
        assert!(absolute.is_absolute());
        assert!(absolute.ends_with(request()));
        assert!(measured_usage.bytes > 0);

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: measured_usage.bytes,
            ..AssetLoadLimits::default()
        })
        .expect("valid exact budget");
        absolute_path(request(), &mut exact).expect("materialize exact absolute path");
        assert_eq!(exact.usage().bytes, measured_usage.bytes);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: measured_usage.bytes - 1,
            ..AssetLoadLimits::default()
        })
        .expect("valid one-short budget");
        let (rejected, error) =
            absolute_path(request(), &mut one_short).expect_err("reject one-short budget");
        assert_eq!(rejected, request());
        assert!(matches!(
            *error,
            WorkspaceError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert_eq!(one_short.usage(), Default::default());
    }

    #[test]
    fn caller_owned_root_image_is_retained_without_reopening_the_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.resource");
        fs::write(&path, b"on disk").unwrap();
        let supplied: Arc<[u8]> = Arc::from(&b"supplied"[..]);
        let mut workspace = AssetWorkspace::new().unwrap();

        let source = workspace
            .load_source_bytes(
                SourceOpenRequest::from_path(&path).unwrap(),
                Arc::clone(&supplied),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(
            workspace
                .state()
                .store()
                .get(source)
                .unwrap()
                .image()
                .as_bytes(),
            supplied.as_ref()
        );
    }

    #[test]
    fn budgeted_root_image_transfers_without_duplicate_arc_accounting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.resource");
        fs::write(&path, b"four").unwrap();
        let image: Arc<[u8]> = Arc::from(&b"four"[..]);
        let request = || SourceOpenRequest::from_path(&path).unwrap();

        let mut direct_budget = AssetLoadBudget::default();
        AssetWorkspace::new()
            .unwrap()
            .load_source_bytes(request(), Arc::clone(&image), &mut direct_budget)
            .unwrap();

        let mut transferred_budget = AssetLoadBudget::default();
        let budgeted =
            BudgetedSourceBytes::from_arc(Arc::clone(&image), &mut transferred_budget).unwrap();
        AssetWorkspace::new()
            .unwrap()
            .load_budgeted_source_bytes(request(), budgeted, &mut transferred_budget)
            .unwrap();

        assert_eq!(transferred_budget.usage(), direct_budget.usage());
    }

    #[test]
    fn budgeted_root_image_rejects_a_different_budget_domain_before_accounting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.resource");
        fs::write(&path, b"four").unwrap();
        let mut source_budget = AssetLoadBudget::default();
        let image =
            BudgetedSourceBytes::from_arc(Arc::from(&b"four"[..]), &mut source_budget).unwrap();
        let mut load_budget = AssetLoadBudget::default();

        let error = AssetWorkspace::new()
            .unwrap()
            .load_budgeted_source_bytes(
                SourceOpenRequest::from_path(&path).unwrap(),
                image,
                &mut load_budget,
            )
            .unwrap_err();

        assert!(matches!(
            error,
            WorkspaceError::Budget(BudgetError::DomainMismatch {
                resource: "source bytes"
            })
        ));
        assert_eq!(load_budget.usage(), Default::default());
    }

    #[test]
    fn admission_conflict_index_retention_is_budgeted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.resource");
        fs::write(&path, b"payload").unwrap();
        let run = |budget: &mut AssetLoadBudget| -> Result<_, WorkspaceError> {
            let workspace = AssetWorkspace::new().unwrap();
            let alias = SourceAlias::new("first.resource").unwrap();
            let origin = PhysicalOrigin::from_existing_path_budgeted(&path, budget)
                .map_err(physical_origin_workspace_error)?;
            let mut indexes = AdmissionConflictIndexes::reserve(2, budget)?;
            let source =
                SourceId::new(workspace.workspace_id(), SourceKind::StreamedResource, 1).unwrap();
            let before_index = budget.usage().bytes;
            indexes
                .prepare_insertion(&alias, &origin, budget)
                .map_err(|error| match error {
                    AdmissionIndexKeyError::Alias(error)
                    | AdmissionIndexKeyError::Origin(error) => *error,
                })?
                .insert(0, source);
            Ok((before_index, indexes, alias, origin, source))
        };

        let mut measured = AssetLoadBudget::default();
        let (before_index, indexes, alias, origin, source) = run(&mut measured).unwrap();
        assert!(measured.usage().bytes > before_index);
        assert_eq!(indexes.aliases.get(&alias), Some(&(0, source)));
        assert_eq!(indexes.origins.get(&origin), Some(&(0, source)));
        let usage = measured.usage();

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        run(&mut exact).unwrap();
        assert_eq!(exact.usage().bytes, usage.bytes);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            run(&mut one_short),
            Err(WorkspaceError::Budget(_))
        ));
        assert!(one_short.usage().bytes < usage.bytes);
    }

    #[test]
    fn budgeted_yaml_root_does_not_charge_its_source_backing_twice() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.prefab");
        let encoded: Arc<[u8]> = Arc::from(b"root: value\n".as_slice());
        fs::write(&path, encoded.as_ref()).unwrap();
        let request = || SourceOpenRequest::from_path(&path).unwrap();

        let mut direct_budget = AssetLoadBudget::default();
        AssetWorkspace::new()
            .unwrap()
            .load_source_bytes(request(), Arc::clone(&encoded), &mut direct_budget)
            .unwrap();

        let mut transferred_budget = AssetLoadBudget::default();
        let image =
            BudgetedSourceBytes::from_arc(Arc::clone(&encoded), &mut transferred_budget).unwrap();
        AssetWorkspace::new()
            .unwrap()
            .load_budgeted_source_bytes(request(), image, &mut transferred_budget)
            .unwrap();

        assert_eq!(transferred_budget.usage(), direct_budget.usage());
    }

    #[test]
    fn workspace_binary_errors_preserve_the_standard_source_chain() {
        let root = WorkspaceError::from(
            unity_asset_binary::error::BinaryError::ResourceLimitExceeded("hard limit".to_owned()),
        );
        assert!(
            std::error::Error::source(&root)
                .and_then(|source| {
                    source.downcast_ref::<unity_asset_binary::error::BinaryError>()
                })
                .is_some_and(|source| matches!(source, unity_asset_binary::error::BinaryError::ResourceLimitExceeded(message) if message == "hard limit"))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::{DigestV1, WorkspaceId};
    use unity_asset_write::artifact::PreparedArtifactSourceCompatibilityError;

    fn budget_with(bytes: u64) -> AssetLoadBudget {
        AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes: bytes.max(1),
            ..unity_asset_core::AssetLoadLimits::default()
        })
        .unwrap()
    }

    fn unload_operation(local: u128) -> SourceAdmissionOperation {
        SourceAdmissionOperation::Unload(
            SourceId::new(
                unity_asset_core::WorkspaceId::from_u128(1).unwrap(),
                unity_asset_core::SourceKind::SerializedFile,
                local,
            )
            .unwrap(),
        )
    }

    fn overallocated_load_operation() -> (SourceAdmissionOperation, u64) {
        let mut path = PathBuf::with_capacity(4_096);
        path.push("payload.resource");
        let path_bytes = path.capacity();
        let mut alias = String::with_capacity(2_048);
        alias.push_str("payload.resource");
        let alias_bytes = alias.capacity();
        let request = SourceOpenRequest::new(path, SourceAlias::new(alias).unwrap());
        (
            SourceAdmissionOperation::LoadPath(request),
            u64::try_from(path_bytes + alias_bytes).unwrap(),
        )
    }

    #[test]
    fn batch_capacity_is_caller_budgeted_and_never_grows() {
        let mut measured_budget = AssetLoadBudget::default();
        let measured = SourceAdmissionBatch::with_capacity(2, &mut measured_budget).unwrap();
        let measured_bytes = measured_budget.usage().bytes;
        assert!(measured_bytes > 0);
        drop(measured);

        let mut one_short = budget_with(measured_bytes - 1);
        assert!(matches!(
            SourceAdmissionBatch::with_capacity(2, &mut one_short),
            Err(SourceAdmissionBatchAllocationError::Budget(
                BudgetError::Exceeded {
                    resource: "bytes",
                    ..
                }
            ))
        ));
        assert_eq!(one_short.usage().bytes, 0);

        let mut exact = budget_with(measured_bytes);
        let mut batch = SourceAdmissionBatch::with_capacity(2, &mut exact).unwrap();
        batch.try_push(unload_operation(1), &mut exact).unwrap();
        batch.try_push(unload_operation(2), &mut exact).unwrap();
        let error = batch.try_push(unload_operation(3), &mut exact).unwrap_err();
        let capacity = error.capacity_error().unwrap();
        assert_eq!(capacity.capacity(), 2);
        assert_eq!(capacity.requested(), 3);
        assert_eq!(batch.len(), 2);
        assert_eq!(exact.usage().bytes, measured_bytes);

        let other_budget = AssetLoadBudget::default();
        assert!(matches!(
            batch.validate_budget(&other_budget),
            Err(BudgetError::DomainMismatch {
                resource: "source admission batch"
            })
        ));
    }

    #[test]
    fn batch_push_exactly_accounts_nested_request_backing() {
        let mut measured = AssetLoadBudget::default();
        let batch = SourceAdmissionBatch::with_capacity(1, &mut measured).unwrap();
        let batch_bytes = measured.usage().bytes;
        drop(batch);
        let (_, operation_bytes) = overallocated_load_operation();

        let mut one_short = budget_with(batch_bytes + operation_bytes - 1);
        let mut rejected = SourceAdmissionBatch::with_capacity(1, &mut one_short).unwrap();
        assert!(matches!(
            rejected.try_push(overallocated_load_operation().0, &mut one_short),
            Err(SourceAdmissionBatchPushError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit == batch_bytes + operation_bytes - 1
                && requested == batch_bytes + operation_bytes
        ));
        assert_eq!(one_short.usage().bytes, batch_bytes);
        assert!(rejected.is_empty());

        let mut exact = budget_with(batch_bytes + operation_bytes);
        let mut accepted = SourceAdmissionBatch::with_capacity(1, &mut exact).unwrap();
        accepted
            .try_push(overallocated_load_operation().0, &mut exact)
            .unwrap();
        assert_eq!(accepted.len(), 1);
        assert_eq!(exact.usage().bytes, batch_bytes + operation_bytes);
    }

    #[test]
    fn batch_width_is_bounded_by_entry_budget() {
        let limits = unity_asset_core::AssetLoadLimits {
            max_entries: 1,
            ..unity_asset_core::AssetLoadLimits::default()
        };
        let mut one_short = AssetLoadBudget::new(limits).unwrap();
        let mut rejected = SourceAdmissionBatch::with_capacity(2, &mut one_short).unwrap();
        rejected
            .try_push(unload_operation(1), &mut one_short)
            .unwrap();
        assert!(matches!(
            rejected.try_push(unload_operation(2), &mut one_short),
            Err(SourceAdmissionBatchPushError::Budget(
                BudgetError::Exceeded {
                    resource: "entries",
                    limit: 1,
                    requested: 2,
                }
            ))
        ));
        assert_eq!(rejected.len(), 1);
        assert_eq!(one_short.usage().entries, 1);

        let mut exact = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_entries: 2,
            ..unity_asset_core::AssetLoadLimits::default()
        })
        .unwrap();
        let mut accepted = SourceAdmissionBatch::with_capacity(2, &mut exact).unwrap();
        accepted.try_push(unload_operation(1), &mut exact).unwrap();
        accepted.try_push(unload_operation(2), &mut exact).unwrap();
        assert_eq!(accepted.len(), 2);
        assert_eq!(exact.usage().entries, 2);
        assert_eq!(exact.usage().members, 0);
    }

    #[test]
    fn tolerant_policy_has_an_explicit_content_only_allowlist() {
        let policy = SourceAdmissionPolicy::TolerantContent;
        for category in [
            SourceAdmissionErrorCategory::Content,
            SourceAdmissionErrorCategory::Unsupported,
            SourceAdmissionErrorCategory::Identity,
            SourceAdmissionErrorCategory::DuplicateAlias,
            SourceAdmissionErrorCategory::DuplicatePhysicalOrigin,
        ] {
            assert!(policy.tolerates(category));
        }
        for category in [
            SourceAdmissionErrorCategory::Budget,
            SourceAdmissionErrorCategory::Io,
            SourceAdmissionErrorCategory::Allocation,
            SourceAdmissionErrorCategory::SourceChanged,
            SourceAdmissionErrorCategory::Contract,
            SourceAdmissionErrorCategory::ResourceLimit,
            SourceAdmissionErrorCategory::WorkspaceInvariant,
        ] {
            assert!(!policy.tolerates(category));
        }
    }

    #[test]
    fn archive_io_classification_separates_content_from_runtime_failures() {
        let malformed = ArchiveLoadError::ReadMember {
            wire_ordinal: 0,
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed stream"),
        };
        let denied = ArchiveLoadError::ReadMember {
            wire_ordinal: 0,
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "runtime failure"),
        };
        let allocation = ArchiveLoadError::ReadMember {
            wire_ordinal: 0,
            source: std::io::Error::new(std::io::ErrorKind::OutOfMemory, "allocation failure"),
        };

        assert_eq!(
            classify_archive_error(&malformed),
            SourceAdmissionErrorCategory::Content
        );
        assert_eq!(
            classify_archive_error(&denied),
            SourceAdmissionErrorCategory::Io
        );
        assert_eq!(
            classify_archive_error(&allocation),
            SourceAdmissionErrorCategory::Allocation
        );
    }

    #[test]
    fn typed_workspace_allocation_is_never_downgraded() {
        let failure = SourceAdmissionFailure::from(WorkspaceError::Allocation {
            resource: "yaml mapping",
            requested: 4,
            unit: super::super::view::WorkspaceAllocationUnit::Slots,
            message: "capacity rejected".to_owned(),
        });

        assert_eq!(failure.category(), SourceAdmissionErrorCategory::Allocation);
        assert!(!SourceAdmissionPolicy::TolerantContent.tolerates(failure.category()));
    }

    #[test]
    fn prepared_artifact_compatibility_failure_is_never_tolerated() {
        let source_id =
            SourceId::new(WorkspaceId::from_u128(0x51).unwrap(), SourceKind::Yaml, 1).unwrap();
        let failure = SourceAdmissionFailure::from(WorkspaceError::from(
            PreparedArtifactSourceCompatibilityError::DigestMismatch {
                source_id,
                expected: DigestV1::hash_bytes(b"expected"),
                actual: DigestV1::hash_bytes(b"actual"),
            },
        ));

        assert_eq!(
            failure.category(),
            SourceAdmissionErrorCategory::WorkspaceInvariant
        );
        assert!(!SourceAdmissionPolicy::TolerantContent.tolerates(failure.category()));
    }
}
