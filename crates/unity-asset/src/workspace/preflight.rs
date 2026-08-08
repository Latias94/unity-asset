//! Transactional, zero-write mutation preflight.

use std::collections::TryReserveError;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, Diagnostic, DigestV1, ObjectId, SourceFingerprint, SourceId,
    SourceLocator, WorkspaceId, WorkspaceRevision, vec_allocation_bytes,
};
use unity_asset_write::artifact::{
    ArtifactBudgetUsage, ArtifactBuildError, ArtifactLimits, OutputSlot, PreparedArtifactSet,
};

use super::overlay::{PreparedState, PreparedView};
use super::{AssetWorkspace, MutationPlan};

mod artifact_graph;
pub(crate) mod destination;
mod reference;
mod resource;
mod runner;
pub(crate) mod source_proof;
mod yaml;

#[cfg(test)]
mod scenario_tests;

/// Wire schema shared by success and rejection reports.
pub const PREPARE_REPORT_VERSION: u8 = 2;

/// Caller-selected proof-image ceilings for one zero-write prepare operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrepareOptions {
    artifact_limits: ArtifactLimits,
}

impl PrepareOptions {
    #[must_use]
    pub const fn new(artifact_limits: ArtifactLimits) -> Self {
        Self { artifact_limits }
    }

    #[must_use]
    pub const fn artifact_limits(self) -> ArtifactLimits {
        self.artifact_limits
    }
}

impl Default for PrepareOptions {
    fn default() -> Self {
        Self::new(ArtifactLimits::default())
    }
}

/// Deterministic phase in which prepare accepted or rejected an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrepareStage {
    PlanIdentity,
    SourceValidation,
    AddressResolution,
    Mutation,
    ResourceAllocation,
    ArtifactDeclaration,
    ArtifactEncoding,
    IndependentReparse,
    DestinationValidation,
    PreparedView,
}

/// One ordered, structured prepare rejection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrepareDiagnostic {
    ordinal: Option<u32>,
    stage: PrepareStage,
    diagnostic: Diagnostic,
    source: Option<SourceLocator>,
    expected_fingerprint: Option<SourceFingerprint>,
    actual_fingerprint: Option<SourceFingerprint>,
}

impl PrepareDiagnostic {
    #[must_use]
    pub const fn ordinal(&self) -> Option<u32> {
        self.ordinal
    }

    #[must_use]
    pub const fn stage(&self) -> PrepareStage {
        self.stage
    }

    #[must_use]
    pub const fn diagnostic(&self) -> &Diagnostic {
        &self.diagnostic
    }

    #[must_use]
    pub const fn source(&self) -> Option<&SourceLocator> {
        self.source.as_ref()
    }

    #[must_use]
    pub const fn expected_fingerprint(&self) -> Option<SourceFingerprint> {
        self.expected_fingerprint
    }

    #[must_use]
    pub const fn actual_fingerprint(&self) -> Option<SourceFingerprint> {
        self.actual_fingerprint
    }
}

/// Complete zero-write rejection report. No prepared view exists when this value is returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrepareFailureReport {
    version: u8,
    workspace_id: WorkspaceId,
    observed_revision: WorkspaceRevision,
    plan_digest: Option<DigestV1>,
    diagnostics: Vec<PrepareDiagnostic>,
}

impl PrepareFailureReport {
    #[must_use]
    pub const fn version(&self) -> u8 {
        self.version
    }

    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    #[must_use]
    pub const fn observed_revision(&self) -> WorkspaceRevision {
        self.observed_revision
    }

    #[must_use]
    pub const fn plan_digest(&self) -> Option<DigestV1> {
        self.plan_digest
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[PrepareDiagnostic] {
        &self.diagnostics
    }
}

/// Error returned when a workspace cannot construct a complete prepared change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareError {
    report: PrepareFailureReport,
}

impl PrepareError {
    #[must_use]
    pub const fn report(&self) -> &PrepareFailureReport {
        &self.report
    }

    #[must_use]
    pub fn into_report(self) -> PrepareFailureReport {
        self.report
    }
}

impl fmt::Display for PrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.report.diagnostics.first() {
            Some(first) => write!(
                formatter,
                "workspace prepare rejected during {:?}: {}",
                first.stage,
                first.diagnostic.message()
            ),
            None => formatter.write_str("workspace prepare rejected without a diagnostic"),
        }
    }
}

impl std::error::Error for PrepareError {}

/// Exact artifact-graph costs and proof pass counters for one successful prepare.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PrepareArtifactReport {
    outputs: u64,
    proof_images: u64,
    publication_bytes: u64,
    proof_bytes: u64,
    generated_bytes: u64,
    metadata_bytes: u64,
    pinned_source_bytes: u64,
    retained_bytes: u64,
    referenced_source_bytes: u64,
    segments: u64,
    source_ranges: u64,
    generated_chunks: u64,
    digest_passes: u64,
    digest_reuses: u64,
    validation_passes: u64,
    peak_scratch_bytes: u64,
}

macro_rules! scalar_getters {
    ($($name:ident),+ $(,)?) => {
        $(
            #[must_use]
            pub const fn $name(self) -> u64 {
                self.$name
            }
        )+
    };
}

impl PrepareArtifactReport {
    scalar_getters!(
        outputs,
        proof_images,
        publication_bytes,
        proof_bytes,
        generated_bytes,
        metadata_bytes,
        pinned_source_bytes,
        retained_bytes,
        referenced_source_bytes,
        segments,
        source_ranges,
        generated_chunks,
        digest_passes,
        digest_reuses,
        validation_passes,
        peak_scratch_bytes,
    );
}

/// Logical source change and the exact artifact that proves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreparedSourceReport {
    source_id: SourceId,
    locator: SourceLocator,
    physical_domain_owner: SourceId,
    base_fingerprint: Option<SourceFingerprint>,
    prepared_fingerprint: SourceFingerprint,
    artifact_digest: DigestV1,
    artifact_bytes: u64,
    logical_changed_bytes: u64,
    physical_rewrite_bytes: u64,
    publication_root: bool,
}

impl PreparedSourceReport {
    #[must_use]
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    #[must_use]
    pub const fn locator(&self) -> &SourceLocator {
        &self.locator
    }

    #[must_use]
    pub const fn physical_domain_owner(&self) -> SourceId {
        self.physical_domain_owner
    }

    #[must_use]
    pub const fn base_fingerprint(&self) -> Option<SourceFingerprint> {
        self.base_fingerprint
    }

    #[must_use]
    pub const fn prepared_fingerprint(&self) -> SourceFingerprint {
        self.prepared_fingerprint
    }

    #[must_use]
    pub const fn artifact_digest(&self) -> DigestV1 {
        self.artifact_digest
    }

    #[must_use]
    pub const fn artifact_bytes(&self) -> u64 {
        self.artifact_bytes
    }

    #[must_use]
    pub const fn logical_changed_bytes(&self) -> u64 {
        self.logical_changed_bytes
    }

    #[must_use]
    pub const fn physical_rewrite_bytes(&self) -> u64 {
        self.physical_rewrite_bytes
    }

    #[must_use]
    pub const fn publication_root(&self) -> bool {
        self.publication_root
    }
}

/// Deterministic success report bound to the exact Prepared View revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrepareReport {
    version: u8,
    workspace_id: WorkspaceId,
    base_revision: WorkspaceRevision,
    prepared_revision: WorkspaceRevision,
    plan_digest: DigestV1,
    operation_count: u32,
    sources: Vec<PreparedSourceReport>,
    artifacts: PrepareArtifactReport,
}

impl PrepareReport {
    #[must_use]
    pub const fn version(&self) -> u8 {
        self.version
    }

    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    #[must_use]
    pub const fn base_revision(&self) -> WorkspaceRevision {
        self.base_revision
    }

    #[must_use]
    pub const fn prepared_revision(&self) -> WorkspaceRevision {
        self.prepared_revision
    }

    #[must_use]
    pub const fn plan_digest(&self) -> DigestV1 {
        self.plan_digest
    }

    #[must_use]
    pub const fn operation_count(&self) -> u32 {
        self.operation_count
    }

    #[must_use]
    pub fn sources(&self) -> &[PreparedSourceReport] {
        &self.sources
    }

    #[must_use]
    pub const fn artifacts(&self) -> PrepareArtifactReport {
        self.artifacts
    }
}

/// Opaque, single-candidate result of a complete zero-write proof.
///
/// The prepared view and report retain the same immutable candidate revision. Durable publication
/// consumes this value in the commit unit; callers cannot construct a partial change.
pub struct PreparedChange {
    state: Arc<PreparedState>,
    report: PrepareReport,
    artifact_usage: ArtifactBudgetUsage,
    logical_changes: PreparedLogicalChanges,
    source_proofs: source_proof::PhysicalDependencyProofSet,
    publications: PreparedPublicationSet,
}

impl PreparedChange {
    #[must_use]
    pub fn view(&self) -> PreparedView {
        PreparedView::new(Arc::clone(&self.state))
    }

    #[must_use]
    pub const fn report(&self) -> &PrepareReport {
        &self.report
    }

    #[must_use]
    pub const fn artifact_usage(&self) -> ArtifactBudgetUsage {
        self.artifact_usage
    }

    pub(crate) fn artifacts(&self) -> &Arc<PreparedArtifactSet> {
        self.state.artifacts()
    }

    pub(crate) const fn state(&self) -> &Arc<PreparedState> {
        &self.state
    }

    pub(crate) const fn logical_changes(&self) -> &PreparedLogicalChanges {
        &self.logical_changes
    }

    pub(crate) const fn publications(&self) -> &PreparedPublicationSet {
        &self.publications
    }

    pub(crate) fn revalidate_publication_inputs(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), PreparedInputProofError> {
        self.source_proofs
            .revalidate(budget)
            .map_err(|error| PreparedInputProofError::Source(Box::new(error)))?;
        self.publications.revalidate(budget)?;
        Ok(())
    }
}

impl fmt::Debug for PreparedChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedChange")
            .field("workspace_id", &self.report.workspace_id)
            .field("base_revision", &self.report.base_revision)
            .field("prepared_revision", &self.report.prepared_revision)
            .field("plan_digest", &self.report.plan_digest)
            .field("operation_count", &self.report.operation_count)
            .field("source_count", &self.report.sources.len())
            .field(
                "changed_source_count",
                &self.logical_changes.sources().len(),
            )
            .field(
                "changed_object_count",
                &self.logical_changes.objects().len(),
            )
            .field("artifact_usage", &self.artifact_usage)
            .field("source_proof_count", &self.source_proofs.bindings().len())
            .field("publication_count", &self.publications.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub(crate) struct PreparedLogicalChanges {
    sources: Vec<SourceId>,
    objects: Vec<ObjectId>,
}

impl PreparedLogicalChanges {
    /// Projects operation-touched objects onto the sources whose prepared bytes actually changed.
    ///
    /// This is prepare-time semantic projection. `ChangeSet` separately validates arbitrary
    /// public wire input when the commit transaction and revisions are available.
    pub(super) fn from_actual_sources_and_touched_objects(
        mut sources: Vec<SourceId>,
        mut objects: Vec<ObjectId>,
    ) -> Self {
        sources.sort_unstable();
        sources.dedup();
        objects.sort_unstable();
        objects.dedup();

        let mut source_index = 0;
        objects.retain(|object| {
            while sources
                .get(source_index)
                .is_some_and(|source| *source < object.source())
            {
                source_index += 1;
            }
            sources.get(source_index).copied() == Some(object.source())
        });
        Self { sources, objects }
    }

    #[must_use]
    pub(crate) fn sources(&self) -> &[SourceId] {
        &self.sources
    }

    #[must_use]
    pub(crate) fn objects(&self) -> &[ObjectId] {
        &self.objects
    }
}

#[derive(Debug, Error)]
pub(crate) enum PreparedInputProofError {
    #[error(transparent)]
    Source(Box<source_proof::PhysicalDependencyProofError>),
    #[error(transparent)]
    Destination(#[from] destination::DestinationProofError),
}

/// Complete output/source/artifact/destination authority minted by prepare.
#[derive(Debug)]
pub(crate) struct PreparedPublicationSet {
    destinations: destination::DestinationProofSet,
    source_order: Vec<usize>,
}

impl PreparedPublicationSet {
    pub(super) fn seal(
        destinations: destination::DestinationProofSet,
        state: &PreparedState,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, PreparedPublicationError> {
        let bindings = destinations.bindings();
        if bindings.len() != state.artifacts().len() {
            return Err(PreparedPublicationError::CountMismatch {
                outputs: state.artifacts().len(),
                destinations: bindings.len(),
            });
        }
        let member_work = checked_publication_authority_members(
            bindings.len(),
            state.core().source_bindings().len(),
        )?;
        budget.check_members(member_work)?;
        budget.consume_members(member_work)?;

        let mut previous_name = None;
        for (output_ordinal, destination) in bindings.iter().enumerate() {
            let source_id = destination.source();
            let output_slot = destination.output();
            let output = state.artifacts().output(output_slot).map_err(|error| {
                PreparedPublicationError::Artifact {
                    output: output_ordinal,
                    source: error,
                }
            })?;
            let name = output.name().as_str();
            if previous_name.is_some_and(|previous| previous >= name) {
                return Err(PreparedPublicationError::NonCanonicalOutputOrder {
                    output: output_ordinal,
                });
            }
            previous_name = Some(name);
            let source = state.core().source_binding(source_id).ok_or(
                PreparedPublicationError::SourceBindingMissing {
                    output: output_ordinal,
                    source_id,
                },
            )?;
            if !source.is_publication_root() || source.artifact() != output.handle() {
                return Err(PreparedPublicationError::SourceBindingMismatch {
                    output: output_ordinal,
                    source_id,
                });
            }
        }
        let mut source_order = budgeted_publication_index_vec(bindings.len(), budget)?;
        source_order.extend(0..bindings.len());
        source_order
            .sort_unstable_by_key(|ordinal| (binding_source(&bindings[*ordinal]), *ordinal));
        for pair in source_order.windows(2) {
            if binding_source(&bindings[pair[0]]) == binding_source(&bindings[pair[1]]) {
                return Err(PreparedPublicationError::DuplicateSource {
                    source_id: binding_source(&bindings[pair[0]]),
                });
            }
        }
        Ok(Self {
            destinations,
            source_order,
        })
    }

    #[must_use]
    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = PreparedPublicationRef<'_>> {
        self.destinations
            .bindings()
            .iter()
            .map(|binding| PreparedPublicationRef { binding })
    }

    #[must_use]
    pub(crate) const fn len(&self) -> usize {
        self.source_order.len()
    }

    #[must_use]
    pub(crate) fn ordinal_for_source(&self, source: SourceId) -> Option<usize> {
        self.source_order
            .binary_search_by_key(&source, |ordinal| {
                binding_source(&self.destinations.bindings()[*ordinal])
            })
            .ok()
            .map(|index| self.source_order[index])
    }

    pub(crate) fn revalidate(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), destination::DestinationProofError> {
        self.destinations.revalidate(budget)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PreparedPublicationRef<'publication> {
    binding: &'publication destination::DestinationProof,
}

impl<'publication> PreparedPublicationRef<'publication> {
    #[must_use]
    pub(crate) fn source(self) -> SourceId {
        binding_source(self.binding)
    }

    #[must_use]
    pub(crate) fn output(self) -> OutputSlot {
        self.binding.output()
    }

    #[must_use]
    pub(crate) fn target(self) -> &'publication Path {
        self.binding.target()
    }

    #[must_use]
    pub(crate) const fn expected(self) -> destination::DestinationState {
        self.binding.expected()
    }

    #[must_use]
    pub(crate) const fn existing_file_identity(
        self,
    ) -> Option<&'publication super::source_catalog::PhysicalFileIdentity> {
        self.binding.existing_file_identity()
    }

    #[must_use]
    pub(crate) const fn destination_parent_identity(
        self,
    ) -> &'publication super::source_catalog::PhysicalFileIdentity {
        self.binding.destination_parent_identity()
    }

    #[must_use]
    pub(crate) fn filesystem_anchor(self) -> &'publication Path {
        self.binding.filesystem_anchor()
    }
}

fn binding_source(binding: &destination::DestinationProof) -> SourceId {
    binding.source()
}

fn checked_publication_authority_members(
    publication_count: usize,
    source_binding_count: usize,
) -> Result<u64, BudgetError> {
    // Cover the validation scan, indexed source lookup, index population/duplicate scan, and a
    // conservative two-comparison allowance per unstable-sort level.
    let publications =
        u64::try_from(publication_count).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "prepared publication authority members",
        })?;
    let source_lookup_levels = binary_search_levels(source_binding_count)?;
    let sort_levels = sort_levels(publication_count)?;
    let work_per_publication = source_lookup_levels
        .checked_add(
            sort_levels
                .checked_mul(2)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "prepared publication authority members",
                })?,
        )
        .and_then(|work| work.checked_add(3))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "prepared publication authority members",
        })?;
    publications
        .checked_mul(work_per_publication)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "prepared publication authority members",
        })
}

fn binary_search_levels(count: usize) -> Result<u64, BudgetError> {
    let count = u64::try_from(count).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "prepared publication authority members",
    })?;
    Ok(if count == 0 {
        0
    } else {
        u64::from(count.ilog2()) + 1
    })
}

fn sort_levels(count: usize) -> Result<u64, BudgetError> {
    let count = u64::try_from(count).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "prepared publication authority members",
    })?;
    Ok(if count <= 1 {
        0
    } else {
        u64::from((count - 1).ilog2()) + 1
    })
}

fn budgeted_publication_index_vec(
    capacity: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<usize>, PreparedPublicationError> {
    let entries = u64::try_from(capacity).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "prepared publication source index",
    })?;
    let minimum =
        vec_allocation_bytes::<usize>(capacity).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "prepared publication source index",
        })?;
    budget.check_entries(entries)?;
    budget.check_bytes(minimum)?;
    let mut order = Vec::new();
    order.try_reserve_exact(capacity).map_err(|error| {
        PreparedPublicationError::IndexAllocation {
            requested: capacity,
            source: error,
        }
    })?;
    let retained = vec_allocation_bytes::<usize>(order.capacity()).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "prepared publication source index",
        }
    })?;
    budget.check_bytes(retained)?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(retained)?;
    Ok(order)
}

#[derive(Debug, Error)]
pub(super) enum PreparedPublicationError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("failed to allocate {requested} prepared publication source indexes")]
    IndexAllocation {
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error(
        "prepared publication counts disagree: {outputs} outputs and {destinations} destinations"
    )]
    CountMismatch { outputs: usize, destinations: usize },
    #[error("prepared publication output {output} has no proven source binding for {source_id:?}")]
    SourceBindingMissing { output: usize, source_id: SourceId },
    #[error("prepared publication output {output} is not in strict logical-name order")]
    NonCanonicalOutputOrder { output: usize },
    #[error("prepared publication output {output} does not belong to source {source_id:?}")]
    SourceBindingMismatch { output: usize, source_id: SourceId },
    #[error("prepared publication source {source_id:?} owns more than one output")]
    DuplicateSource { source_id: SourceId },
    #[error("prepared publication output {output} has an invalid artifact capability")]
    Artifact {
        output: usize,
        #[source]
        source: ArtifactBuildError,
    },
}

impl AssetWorkspace {
    /// Builds and independently proves one read-your-writes candidate without durable writes.
    pub fn prepare(
        &self,
        plan: MutationPlan,
        options: PrepareOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedChange, PrepareError> {
        runner::prepare(self, plan, options, budget)
    }
}
