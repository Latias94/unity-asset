//! Transactional, zero-write mutation preflight.

use std::fmt;
use std::sync::Arc;

use serde::Serialize;
use unity_asset_core::{
    AssetLoadBudget, Diagnostic, DigestV1, ObjectId, SourceFingerprint, SourceId, SourceLocator,
    WorkspaceId, WorkspaceRevision,
};
use unity_asset_write::artifact::{ArtifactBudgetUsage, ArtifactLimits, PreparedArtifactSet};

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
    changed_objects: Vec<ObjectId>,
    source_proofs: source_proof::PhysicalDependencyProofSet,
    destination_proofs: destination::DestinationProofSet,
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

    pub(crate) fn changed_objects(&self) -> &[ObjectId] {
        &self.changed_objects
    }

    pub(crate) const fn source_proofs(&self) -> &source_proof::PhysicalDependencyProofSet {
        &self.source_proofs
    }

    pub(crate) const fn destination_proofs(&self) -> &destination::DestinationProofSet {
        &self.destination_proofs
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
            .field("changed_object_count", &self.changed_objects.len())
            .field("artifact_usage", &self.artifact_usage)
            .field("source_proof_count", &self.source_proofs.bindings().len())
            .field(
                "destination_proof_count",
                &self.destination_proofs.bindings().len(),
            )
            .finish_non_exhaustive()
    }
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
