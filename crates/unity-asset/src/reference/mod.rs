//! Revision-bound reference facts, resolution, and graph queries.

mod builder;
mod cache;
mod fact;
mod index;
mod occurrence;
mod projection;
mod query;
mod resolution;

use std::collections::TryReserveError;
use std::fmt;
use std::sync::Arc;

use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ContractError, Diagnostic, DiagnosticError, FieldPathError,
    ObjectAddress, RevisionedObjectHandle, WorkspaceId, WorkspaceRevision,
};
use unity_asset_yaml::YamlReferenceScanError;

use crate::BinaryError;
use crate::workspace::{WorkspaceError, WorkspaceView};

pub use fact::{
    BinaryExternalReference, RawReferenceTarget, ReferenceFact, ReferenceFormat, ReferenceGuid,
    ReferenceResolution,
};
pub use projection::{
    ReferenceProjectionFormat, ReferenceProjectionOptions, ReferenceProjectionReport,
    ReferenceResolutionCounts,
};
pub use query::{
    ReferenceCycles, ReferenceDirection, ReferenceFacts, ReferenceNodes, ReferenceTraversal,
    ReferenceTraversalLimits, ReferenceTraversalTruncation,
};

pub(crate) use cache::ReferenceStore;
use index::ReferenceIndex;

/// Deterministic soft limits applied after format-local facts are available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReferenceGraphBuildOptions {
    max_nodes: Option<u64>,
    max_facts: Option<u64>,
}

impl ReferenceGraphBuildOptions {
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            max_nodes: None,
            max_facts: None,
        }
    }

    /// Limits nodes retained by the graph index after format-local facts are scanned.
    ///
    /// Source occurrence scanning remains content-complete so its fingerprint cache cannot
    /// publish partial facts. Use [`AssetLoadBudget`] to impose hard work and memory limits.
    #[must_use]
    pub const fn with_max_nodes(mut self, maximum: u64) -> Self {
        self.max_nodes = Some(maximum);
        self
    }

    #[must_use]
    pub const fn with_max_facts(mut self, maximum: u64) -> Self {
        self.max_facts = Some(maximum);
        self
    }

    #[must_use]
    pub const fn max_nodes(self) -> Option<u64> {
        self.max_nodes
    }

    #[must_use]
    pub const fn max_facts(self) -> Option<u64> {
        self.max_facts
    }
}

impl Default for ReferenceGraphBuildOptions {
    fn default() -> Self {
        Self::unbounded()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReferenceTruncationKind {
    Nodes,
    Facts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceTruncation {
    kind: ReferenceTruncationKind,
    limit: u64,
    observed: u64,
}

impl ReferenceTruncation {
    pub(crate) const fn new(kind: ReferenceTruncationKind, limit: u64, observed: u64) -> Self {
        Self {
            kind,
            limit,
            observed,
        }
    }

    #[must_use]
    pub const fn kind(self) -> ReferenceTruncationKind {
        self.kind
    }

    #[must_use]
    pub const fn limit(self) -> u64 {
        self.limit
    }

    #[must_use]
    pub const fn observed(self) -> u64 {
        self.observed
    }
}

/// Coverage evidence retained by every graph, including deliberately partial graphs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceGraphCoverage {
    total_sources: u64,
    scanned_sources: u64,
    total_nodes: u64,
    indexed_nodes: u64,
    fact_count: u64,
    complete: bool,
    truncations: Box<[ReferenceTruncation]>,
}

impl ReferenceGraphCoverage {
    pub(crate) fn new(
        total_sources: u64,
        scanned_sources: u64,
        total_nodes: u64,
        indexed_nodes: u64,
        fact_count: u64,
        complete: bool,
        truncations: Vec<ReferenceTruncation>,
    ) -> Self {
        Self {
            total_sources,
            scanned_sources,
            total_nodes,
            indexed_nodes,
            fact_count,
            complete,
            truncations: truncations.into_boxed_slice(),
        }
    }

    #[must_use]
    pub const fn total_sources(&self) -> u64 {
        self.total_sources
    }

    #[must_use]
    pub const fn scanned_sources(&self) -> u64 {
        self.scanned_sources
    }

    #[must_use]
    pub const fn total_nodes(&self) -> u64 {
        self.total_nodes
    }

    #[must_use]
    pub const fn indexed_nodes(&self) -> u64 {
        self.indexed_nodes
    }

    #[must_use]
    pub const fn fact_count(&self) -> u64 {
        self.fact_count
    }

    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    #[must_use]
    pub const fn truncations(&self) -> &[ReferenceTruncation] {
        &self.truncations
    }
}

/// Non-canonical telemetry for one graph build invocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReferenceGraphBuildStats {
    graph_cache_hit: bool,
    source_occurrence_cache_hits: u64,
}

impl ReferenceGraphBuildStats {
    pub(crate) const fn new(graph_cache_hit: bool, source_occurrence_cache_hits: u64) -> Self {
        Self {
            graph_cache_hit,
            source_occurrence_cache_hits,
        }
    }

    #[must_use]
    pub const fn graph_cache_hit(self) -> bool {
        self.graph_cache_hit
    }

    #[must_use]
    pub const fn source_occurrence_cache_hits(self) -> u64 {
        self.source_occurrence_cache_hits
    }
}

/// Immutable graph whose facts and resolved handles all describe one workspace revision.
#[derive(Debug, Clone)]
pub struct ReferenceGraph {
    inner: Arc<ReferenceIndex>,
    build_stats: ReferenceGraphBuildStats,
}

impl ReferenceGraph {
    pub fn build(
        view: &dyn WorkspaceView,
        options: ReferenceGraphBuildOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ReferenceGraphError> {
        builder::build_graph(view, options, budget)
            .map(|(inner, build_stats)| Self { inner, build_stats })
    }

    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        self.inner.workspace()
    }

    #[must_use]
    pub fn revision(&self) -> WorkspaceRevision {
        self.inner.revision()
    }

    #[must_use]
    pub fn coverage(&self) -> &ReferenceGraphCoverage {
        self.inner.coverage()
    }

    /// Returns execution telemetry excluded from canonical graph projections.
    #[must_use]
    pub const fn build_stats(&self) -> ReferenceGraphBuildStats {
        self.build_stats
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.coverage().is_complete()
    }

    #[must_use]
    pub fn facts(&self) -> &[ReferenceFact] {
        self.inner.facts()
    }

    #[must_use]
    pub fn nodes(&self) -> &[RevisionedObjectHandle] {
        self.inner.nodes()
    }

    /// Returns the portable address for a node in this exact graph revision.
    pub fn address(
        &self,
        node: &RevisionedObjectHandle,
    ) -> Result<&ObjectAddress, ReferenceGraphError> {
        node.validate_context(self.workspace_id(), self.revision())?;
        self.inner
            .address(node.object())
            .ok_or(ReferenceGraphError::ObjectNotIndexed)
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        self.inner.diagnostics()
    }

    pub fn resolution_counts(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceResolutionCounts, ReferenceGraphError> {
        projection::resolution_counts(&self.inner, budget)
    }

    pub fn outgoing(
        &self,
        source: &RevisionedObjectHandle,
    ) -> Result<ReferenceFacts<'_>, ReferenceGraphError> {
        query::outgoing(&self.inner, source)
    }

    pub fn incoming(
        &self,
        target: &RevisionedObjectHandle,
    ) -> Result<ReferenceFacts<'_>, ReferenceGraphError> {
        query::incoming(&self.inner, target)
    }

    #[must_use]
    pub fn roots(&self) -> ReferenceNodes<'_> {
        query::roots(&self.inner)
    }

    #[must_use]
    pub fn leaves(&self) -> ReferenceNodes<'_> {
        query::leaves(&self.inner)
    }

    pub fn closure(
        &self,
        roots: &[RevisionedObjectHandle],
        direction: ReferenceDirection,
        limits: ReferenceTraversalLimits,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceTraversal, ReferenceGraphError> {
        query::closure(&self.inner, roots, direction, limits, budget)
    }

    pub fn cycles(
        &self,
        limits: ReferenceTraversalLimits,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceCycles, ReferenceGraphError> {
        query::cycles(Arc::clone(&self.inner), limits, budget)
    }

    pub fn write_projection<W: std::io::Write + ?Sized>(
        &self,
        output: &mut W,
        options: ReferenceProjectionOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceProjectionReport, ReferenceGraphError> {
        projection::write_projection(&self.inner, output, options, budget)
    }
}

#[derive(Debug, Error)]
pub enum ReferenceGraphError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Binary(#[from] BinaryError),
    #[error(transparent)]
    Diagnostic(#[from] DiagnosticError),
    #[error(transparent)]
    FieldPath(#[from] FieldPathError),
    #[error(transparent)]
    Yaml(#[from] YamlReferenceScanError),
    #[error("failed to allocate {requested} {unit} for {resource}: {source}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        unit: ReferenceAllocationUnit,
        #[source]
        source: TryReserveError,
    },
    #[error("reference graph cache lock is poisoned")]
    CachePoisoned,
    #[error("reference graph invariant failed: {0}")]
    Invariant(&'static str),
    #[error("object is not indexed by this reference graph")]
    ObjectNotIndexed,
    #[error("{format} projection failed after {bytes_written} bytes")]
    ProjectionIo {
        format: ReferenceProjectionFormat,
        bytes_written: u64,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to encode {format} projection")]
    ProjectionEncoding {
        format: ReferenceProjectionFormat,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceAllocationUnit {
    Bytes,
    Elements,
}

impl fmt::Display for ReferenceAllocationUnit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Bytes => "bytes",
            Self::Elements => "elements",
        })
    }
}
