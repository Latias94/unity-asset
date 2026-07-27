//! Ordered, transactional admission of authoritative workspace sources.

use std::collections::TryReserveError;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;
use unity_asset_binary::error::BinaryError;
use unity_asset_core::{
    AssetLoadBudget, AssetLoadBudgetDomainToken, BudgetError, BudgetedSourceBytes, SourceAlias,
    SourceId, WorkspaceRevision, vec_allocation_bytes,
};
use unity_asset_yaml::BudgetedYamlError;

use super::adapter::archive::ArchiveLoadError;
use super::interface::SourceOpenRequest;
use super::source_catalog::PhysicalOriginError;
use super::view::WorkspaceError;

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
            Self::Workspace(_) => None,
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
        | WorkspaceError::PreparedSourceKindMismatch { .. }
        | WorkspaceError::PreparedArtifactKindMismatch { .. }
        | WorkspaceError::PreparedArtifactDigestMismatch { .. }
        | WorkspaceError::PreparedArtifactLengthMismatch { .. }
        | WorkspaceError::PreparedArtifactSourceProvenanceMismatch { .. }
        | WorkspaceError::PreparedArtifactFingerprintProvenanceMismatch { .. } => {
            SourceAdmissionErrorCategory::WorkspaceInvariant
        }
    }
}

fn classify_binary_error(error: &BinaryError) -> SourceAdmissionErrorCategory {
    match error {
        BinaryError::Budget(_) => SourceAdmissionErrorCategory::Budget,
        BinaryError::Io(_) | BinaryError::Timeout(_) => SourceAdmissionErrorCategory::Io,
        BinaryError::MemoryError(_) => SourceAdmissionErrorCategory::Allocation,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
