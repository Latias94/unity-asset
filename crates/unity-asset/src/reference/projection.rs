use std::fmt;
use std::io::{self, Write};

use serde::ser::{Error as _, SerializeSeq, SerializeStruct};
use serde::{Serialize, Serializer};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, FieldPathSegment, WorkspaceId, WorkspaceRevision,
};

use super::fact::{RawReferenceTarget, ReferenceFact, ReferenceGuid, ReferenceResolution};
use super::index::ReferenceIndex;
use super::{ReferenceGraphError, ReferenceTruncationKind};

const PROJECTION_SCHEMA: &str = "unity-asset.reference-graph.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReferenceProjectionFormat {
    DotV1,
    JsonV1,
    JsonLinesV1,
}

impl fmt::Display for ReferenceProjectionFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DotV1 => "dot_v1",
            Self::JsonV1 => "json_v1",
            Self::JsonLinesV1 => "json_lines_v1",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceProjectionOptions {
    format: ReferenceProjectionFormat,
    max_nodes: Option<u64>,
    max_facts: Option<u64>,
    max_diagnostics: Option<u64>,
}

impl ReferenceProjectionOptions {
    #[must_use]
    pub const fn new(format: ReferenceProjectionFormat) -> Self {
        Self {
            format,
            max_nodes: None,
            max_facts: None,
            max_diagnostics: None,
        }
    }

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

    /// Limits graph-level diagnostics written by the projection.
    ///
    /// This is a deterministic soft limit. Caller-owned load budgets remain hard limits and
    /// return [`ReferenceGraphError::Budget`] rather than a successful truncation report.
    #[must_use]
    pub const fn with_max_diagnostics(mut self, maximum: u64) -> Self {
        self.max_diagnostics = Some(maximum);
        self
    }

    #[must_use]
    pub const fn format(self) -> ReferenceProjectionFormat {
        self.format
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceProjectionReport {
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    nodes_written: u64,
    facts_written: u64,
    resolved_edges_written: u64,
    total_nodes: u64,
    total_facts: u64,
    diagnostics_written: u64,
    total_diagnostics: u64,
    bytes_written: u64,
    complete: bool,
    resolution_counts: ReferenceResolutionCounts,
}

impl ReferenceProjectionReport {
    #[must_use]
    pub const fn workspace_id(self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub const fn revision(self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub const fn nodes_written(self) -> u64 {
        self.nodes_written
    }

    #[must_use]
    pub const fn facts_written(self) -> u64 {
        self.facts_written
    }

    /// Returns the number of resolved relationships represented by the selected format.
    ///
    /// JSON formats retain every selected resolved fact. DOT is intentionally lossy and only
    /// emits edges whose source and target nodes are both present in the node projection.
    #[must_use]
    pub const fn resolved_edges_written(self) -> u64 {
        self.resolved_edges_written
    }

    #[must_use]
    pub const fn total_nodes(self) -> u64 {
        self.total_nodes
    }

    #[must_use]
    pub const fn total_facts(self) -> u64 {
        self.total_facts
    }

    #[must_use]
    pub const fn diagnostics_written(self) -> u64 {
        self.diagnostics_written
    }

    #[must_use]
    pub const fn total_diagnostics(self) -> u64 {
        self.total_diagnostics
    }

    #[must_use]
    pub const fn diagnostics_truncated(self) -> bool {
        self.diagnostics_written < self.total_diagnostics
    }

    #[must_use]
    pub const fn bytes_written(self) -> u64 {
        self.bytes_written
    }

    #[must_use]
    pub const fn is_complete(self) -> bool {
        self.complete
    }

    #[must_use]
    pub const fn resolution_counts(self) -> ReferenceResolutionCounts {
        self.resolution_counts
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ReferenceResolutionCounts {
    null: u64,
    resolved: u64,
    unloaded: u64,
    missing: u64,
    ambiguous: u64,
    invalid: u64,
}

impl ReferenceResolutionCounts {
    #[must_use]
    pub const fn null(self) -> u64 {
        self.null
    }

    #[must_use]
    pub const fn resolved(self) -> u64 {
        self.resolved
    }

    #[must_use]
    pub const fn unloaded(self) -> u64 {
        self.unloaded
    }

    #[must_use]
    pub const fn missing(self) -> u64 {
        self.missing
    }

    #[must_use]
    pub const fn ambiguous(self) -> u64 {
        self.ambiguous
    }

    #[must_use]
    pub const fn invalid(self) -> u64 {
        self.invalid
    }
}

pub(crate) fn write_projection<W: Write + ?Sized>(
    index: &ReferenceIndex,
    output: &mut W,
    options: ReferenceProjectionOptions,
    budget: &mut AssetLoadBudget,
) -> Result<ReferenceProjectionReport, ReferenceGraphError> {
    let plan = ProjectionPlan::prepare(index, options, budget)?;
    let mut writer = BudgetWriter::new(output, budget);
    match options.format {
        ReferenceProjectionFormat::JsonV1 => {
            write_json(index, &mut writer, plan)?;
        }
        ReferenceProjectionFormat::JsonLinesV1 => {
            write_json_lines(index, &mut writer, plan)?;
        }
        ReferenceProjectionFormat::DotV1 => write_dot(index, &mut writer, plan)?,
    }
    finish_flush(&mut writer, options.format)?;
    let bytes_written = writer.bytes_written;
    Ok(ReferenceProjectionReport {
        workspace: index.workspace(),
        revision: index.revision(),
        nodes_written: plan.node_count_u64,
        facts_written: plan.fact_count_u64,
        resolved_edges_written: plan.resolved_edges_written(options.format),
        total_nodes: plan.total_nodes,
        total_facts: plan.total_facts,
        diagnostics_written: plan.diagnostic_count_u64,
        total_diagnostics: plan.total_diagnostics,
        bytes_written,
        complete: plan.complete,
        resolution_counts: plan.resolution_counts,
    })
}

pub(crate) fn resolution_counts(
    index: &ReferenceIndex,
    budget: &mut AssetLoadBudget,
) -> Result<ReferenceResolutionCounts, ReferenceGraphError> {
    let fact_count = usize_to_u64(index.facts().len(), "reference resolution counts")?;
    budget.consume_members(fact_count)?;
    analyze_resolutions(index, 0, 0).map(|analysis| analysis.counts)
}

#[derive(Debug, Clone, Copy)]
struct ProjectionPlan {
    node_count: usize,
    fact_count: usize,
    diagnostic_count: usize,
    node_count_u64: u64,
    fact_count_u64: u64,
    diagnostic_count_u64: u64,
    total_nodes: u64,
    total_facts: u64,
    total_diagnostics: u64,
    complete: bool,
    resolution_counts: ReferenceResolutionCounts,
    selected_resolved_edges: u64,
    dot_resolved_edges: u64,
}

impl ProjectionPlan {
    fn prepare(
        index: &ReferenceIndex,
        options: ReferenceProjectionOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ReferenceGraphError> {
        let total_nodes = usize_to_u64(index.nodes().len(), "reference projection nodes")?;
        let total_facts = usize_to_u64(index.facts().len(), "reference projection facts")?;
        let total_diagnostics = usize_to_u64(
            index.diagnostics().len(),
            "reference projection diagnostics",
        )?;
        let node_count_u64 = options.max_nodes.unwrap_or(total_nodes).min(total_nodes);
        let fact_count_u64 = options.max_facts.unwrap_or(total_facts).min(total_facts);
        let diagnostic_count_u64 = options
            .max_diagnostics
            .unwrap_or(total_diagnostics)
            .min(total_diagnostics);
        let output_entries = node_count_u64
            .checked_add(fact_count_u64)
            .and_then(|count| count.checked_add(diagnostic_count_u64))
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "reference projection entries",
            })?;
        let node_count = u64_to_usize(node_count_u64, "reference projection nodes")?;
        let fact_count = u64_to_usize(fact_count_u64, "reference projection facts")?;
        let diagnostic_count =
            u64_to_usize(diagnostic_count_u64, "reference projection diagnostics")?;

        // Preflight both hard limits before charging either ledger or touching the writer.
        budget.check_entries(output_entries)?;
        budget.check_members(total_facts)?;
        budget.consume_entries(output_entries)?;
        budget.consume_members(total_facts)?;

        let resolution_analysis = analyze_resolutions(index, fact_count, node_count)?;
        Ok(Self {
            node_count,
            fact_count,
            diagnostic_count,
            node_count_u64,
            fact_count_u64,
            diagnostic_count_u64,
            total_nodes,
            total_facts,
            total_diagnostics,
            complete: index.coverage().is_complete()
                && node_count_u64 == total_nodes
                && fact_count_u64 == total_facts
                && diagnostic_count_u64 == total_diagnostics,
            resolution_counts: resolution_analysis.counts,
            selected_resolved_edges: resolution_analysis.selected_resolved_edges,
            dot_resolved_edges: resolution_analysis.dot_resolved_edges,
        })
    }

    const fn resolved_edges_written(self, format: ReferenceProjectionFormat) -> u64 {
        match format {
            ReferenceProjectionFormat::DotV1 => self.dot_resolved_edges,
            ReferenceProjectionFormat::JsonV1 | ReferenceProjectionFormat::JsonLinesV1 => {
                self.selected_resolved_edges
            }
        }
    }

    const fn projection_counts(self, format: ReferenceProjectionFormat) -> ProjectionCounts {
        ProjectionCounts {
            nodes_written: self.node_count_u64,
            facts_written: self.fact_count_u64,
            resolved_edges_written: self.resolved_edges_written(format),
            diagnostics_written: self.diagnostic_count_u64,
            total_nodes: self.total_nodes,
            total_facts: self.total_facts,
            total_diagnostics: self.total_diagnostics,
            diagnostics_truncated: self.diagnostic_count_u64 < self.total_diagnostics,
        }
    }
}

fn write_json<W: Write + ?Sized>(
    index: &ReferenceIndex,
    writer: &mut BudgetWriter<'_, W>,
    plan: ProjectionPlan,
) -> Result<(), ReferenceGraphError> {
    let document = JsonDocument {
        schema: PROJECTION_SCHEMA,
        workspace: index.workspace(),
        revision: index.revision(),
        complete: plan.complete,
        coverage: CoverageProjection(index),
        projection: plan.projection_counts(ReferenceProjectionFormat::JsonV1),
        resolution_counts: plan.resolution_counts,
        nodes: NodesProjection {
            index,
            count: plan.node_count,
        },
        facts: FactsProjection {
            index,
            count: plan.fact_count,
        },
        diagnostics: &index.diagnostics()[..plan.diagnostic_count],
    };
    let result = serde_json::to_writer(&mut *writer, &document);
    finish_json(result, writer, ReferenceProjectionFormat::JsonV1)
}

fn write_json_lines<W: Write + ?Sized>(
    index: &ReferenceIndex,
    writer: &mut BudgetWriter<'_, W>,
    plan: ProjectionPlan,
) -> Result<(), ReferenceGraphError> {
    let header = JsonLineHeader {
        kind: "header",
        schema: PROJECTION_SCHEMA,
        workspace: index.workspace(),
        revision: index.revision(),
        complete: plan.complete,
        coverage: CoverageProjection(index),
        projection: plan.projection_counts(ReferenceProjectionFormat::JsonLinesV1),
        resolution_counts: plan.resolution_counts,
    };
    write_json_line(writer, &header)?;
    for address in &index.addresses()[..plan.node_count] {
        write_json_line(
            writer,
            &JsonNodeLine {
                kind: "node",
                object: address,
            },
        )?;
    }
    for fact in &index.facts()[..plan.fact_count] {
        write_json_line(
            writer,
            &JsonFactLine {
                kind: "fact",
                fact: FactProjection { index, fact },
            },
        )?;
    }
    for diagnostic in &index.diagnostics()[..plan.diagnostic_count] {
        write_json_line(
            writer,
            &JsonDiagnosticLine {
                kind: "diagnostic",
                diagnostic,
            },
        )?;
    }
    Ok(())
}

fn write_json_line<W: Write + ?Sized>(
    writer: &mut BudgetWriter<'_, W>,
    value: &impl Serialize,
) -> Result<(), ReferenceGraphError> {
    let result = serde_json::to_writer(&mut *writer, value);
    finish_json(result, writer, ReferenceProjectionFormat::JsonLinesV1)?;
    finish_io(
        writer.write_all(b"\n"),
        writer,
        ReferenceProjectionFormat::JsonLinesV1,
    )
}

fn write_dot<W: Write + ?Sized>(
    index: &ReferenceIndex,
    writer: &mut BudgetWriter<'_, W>,
    plan: ProjectionPlan,
) -> Result<(), ReferenceGraphError> {
    dot_io(
        writeln!(writer, "digraph unity_asset_references {{"),
        writer,
    )?;
    dot_io(writeln!(writer, "  // schema={PROJECTION_SCHEMA}"), writer)?;
    dot_io(
        writeln!(writer, "  // revision={}", index.revision()),
        writer,
    )?;
    let counts = plan.resolution_counts;
    dot_io(
        writeln!(
            writer,
            "  // resolution null={} resolved={} unloaded={} missing={} ambiguous={} invalid={}",
            counts.null,
            counts.resolved,
            counts.unloaded,
            counts.missing,
            counts.ambiguous,
            counts.invalid
        ),
        writer,
    )?;
    dot_io(
        writeln!(
            writer,
            "  // projection nodes={}/{} facts={}/{} resolved_edges={} diagnostics={}/{} diagnostics_truncated={}",
            plan.node_count,
            plan.total_nodes,
            plan.fact_count,
            plan.total_facts,
            plan.dot_resolved_edges,
            plan.diagnostic_count,
            plan.total_diagnostics,
            plan.diagnostic_count_u64 < plan.total_diagnostics,
        ),
        writer,
    )?;
    for diagnostic in &index.diagnostics()[..plan.diagnostic_count] {
        dot_io(write!(writer, "  // diagnostic="), writer)?;
        let result = serde_json::to_writer(&mut *writer, diagnostic);
        finish_json(result, writer, ReferenceProjectionFormat::DotV1)?;
        dot_io(writeln!(writer), writer)?;
    }
    dot_io(writeln!(writer, "  rankdir=LR;"), writer)?;
    for (ordinal, address) in index.addresses()[..plan.node_count].iter().enumerate() {
        dot_io(write!(writer, "  n{ordinal} [label=\""), writer)?;
        write_object_label(writer, address)?;
        dot_io(writeln!(writer, "\"];"), writer)?;
    }

    let mut emitted = 0_u64;
    for fact in &index.facts()[..plan.fact_count] {
        let Some(target) = fact.resolution().resolved() else {
            continue;
        };
        let Some(source_ordinal) = index.node_ordinal(fact.source().object()) else {
            return Err(ReferenceGraphError::Invariant(
                "reference fact owner is absent from the node index",
            ));
        };
        let Some(target_ordinal) = index.node_ordinal(target.object()) else {
            return Err(ReferenceGraphError::Invariant(
                "resolved reference target is absent from the node index",
            ));
        };
        if source_ordinal >= plan.node_count || target_ordinal >= plan.node_count {
            continue;
        }
        dot_io(
            write!(writer, "  n{source_ordinal} -> n{target_ordinal} [label=\""),
            writer,
        )?;
        write_field_path(writer, fact.field_path())?;
        dot_io(writeln!(writer, "\"];"), writer)?;
        emitted = emitted
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "DOT reference edges",
            })?;
    }
    if plan.node_count < index.nodes().len()
        || plan.fact_count < index.facts().len()
        || plan.diagnostic_count < index.diagnostics().len()
    {
        dot_io(
            writeln!(
                writer,
                "  // truncated nodes={}/{} facts={}/{} diagnostics={}/{}",
                plan.node_count,
                index.nodes().len(),
                plan.fact_count,
                index.facts().len(),
                plan.diagnostic_count,
                index.diagnostics().len(),
            ),
            writer,
        )?;
    }
    dot_io(writeln!(writer, "}}"), writer)?;
    if emitted != plan.dot_resolved_edges {
        return Err(ReferenceGraphError::Invariant(
            "DOT projection edge analysis diverged from emitted edges",
        ));
    }
    Ok(())
}

fn write_object_label<W: Write + ?Sized>(
    writer: &mut BudgetWriter<'_, W>,
    address: &unity_asset_core::ObjectAddress,
) -> Result<(), ReferenceGraphError> {
    write_dot_escaped(writer, address.source_locator().root_alias().as_str())?;
    for step in address.source_locator().members() {
        dot_io(
            write!(
                writer,
                "::{}[occurrence={}]:",
                step.container().tag(),
                step.member().same_name_occurrence()
            ),
            writer,
        )?;
        write_dot_escaped(writer, step.name())?;
    }
    dot_io(write!(writer, ":"), writer)?;
    if let Some(path_id) = address.binary_path_id() {
        dot_io(write!(writer, "path_id={path_id}"), writer)
    } else if let Some(anchor) = address.yaml_anchor() {
        dot_io(write!(writer, "anchor="), writer)?;
        write_dot_escaped(writer, anchor)
    } else if let Some(index) = address.yaml_document_ordinal() {
        dot_io(write!(writer, "document={index}"), writer)
    } else {
        Err(ReferenceGraphError::Invariant(
            "reference node has no format-local identity",
        ))
    }
}

fn write_field_path<W: Write + ?Sized>(
    writer: &mut BudgetWriter<'_, W>,
    path: &unity_asset_core::FieldPath,
) -> Result<(), ReferenceGraphError> {
    dot_io(write!(writer, "$"), writer)?;
    for segment in path.segments() {
        match segment {
            FieldPathSegment::Field(name) => {
                dot_io(write!(writer, "."), writer)?;
                write_dot_escaped(writer, name)?;
            }
            FieldPathSegment::Index(index) => {
                dot_io(write!(writer, "[{index}]"), writer)?;
            }
        }
    }
    Ok(())
}

fn write_dot_escaped<W: Write + ?Sized>(
    writer: &mut BudgetWriter<'_, W>,
    value: &str,
) -> Result<(), ReferenceGraphError> {
    for character in value.chars() {
        match character {
            '\\' => dot_io(write!(writer, "\\\\"), writer)?,
            '"' => dot_io(write!(writer, "\\\""), writer)?,
            '\n' => dot_io(write!(writer, "\\n"), writer)?,
            '\r' => dot_io(write!(writer, "\\r"), writer)?,
            '\t' => dot_io(write!(writer, "\\t"), writer)?,
            character if character.is_control() => {
                dot_io(write!(writer, "\\u{:04x}", u32::from(character)), writer)?;
            }
            character => dot_io(write!(writer, "{character}"), writer)?,
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct JsonDocument<'a> {
    schema: &'static str,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    complete: bool,
    coverage: CoverageProjection<'a>,
    projection: ProjectionCounts,
    resolution_counts: ReferenceResolutionCounts,
    nodes: NodesProjection<'a>,
    facts: FactsProjection<'a>,
    diagnostics: &'a [unity_asset_core::Diagnostic],
}

#[derive(Serialize)]
struct JsonLineHeader<'a> {
    kind: &'static str,
    schema: &'static str,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    complete: bool,
    coverage: CoverageProjection<'a>,
    projection: ProjectionCounts,
    resolution_counts: ReferenceResolutionCounts,
}

#[derive(Serialize)]
struct JsonNodeLine<'a> {
    kind: &'static str,
    object: &'a unity_asset_core::ObjectAddress,
}

#[derive(Serialize)]
struct JsonFactLine<'a> {
    kind: &'static str,
    fact: FactProjection<'a>,
}

#[derive(Serialize)]
struct JsonDiagnosticLine<'a> {
    kind: &'static str,
    diagnostic: &'a unity_asset_core::Diagnostic,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct ProjectionCounts {
    nodes_written: u64,
    facts_written: u64,
    resolved_edges_written: u64,
    diagnostics_written: u64,
    total_nodes: u64,
    total_facts: u64,
    total_diagnostics: u64,
    diagnostics_truncated: bool,
}

struct CoverageProjection<'a>(&'a ReferenceIndex);

impl Serialize for CoverageProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let coverage = self.0.coverage();
        let mut output = serializer.serialize_struct("ReferenceCoverage", 7)?;
        output.serialize_field("total_sources", &coverage.total_sources())?;
        output.serialize_field("scanned_sources", &coverage.scanned_sources())?;
        output.serialize_field("total_nodes", &coverage.total_nodes())?;
        output.serialize_field("indexed_nodes", &coverage.indexed_nodes())?;
        output.serialize_field("fact_count", &coverage.fact_count())?;
        output.serialize_field("complete", &coverage.is_complete())?;
        output.serialize_field("truncations", &TruncationProjection(self.0))?;
        output.end()
    }
}

struct TruncationProjection<'a>(&'a ReferenceIndex);

impl Serialize for TruncationProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let truncations = self.0.coverage().truncations();
        let mut sequence = serializer.serialize_seq(Some(truncations.len()))?;
        for truncation in truncations {
            sequence.serialize_element(&TruncationItem {
                kind: match truncation.kind() {
                    ReferenceTruncationKind::Nodes => "nodes",
                    ReferenceTruncationKind::Facts => "facts",
                },
                limit: truncation.limit(),
                observed: truncation.observed(),
            })?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct TruncationItem {
    kind: &'static str,
    limit: u64,
    observed: u64,
}

struct NodesProjection<'a> {
    index: &'a ReferenceIndex,
    count: usize,
}

impl Serialize for NodesProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.count))?;
        for address in &self.index.addresses()[..self.count] {
            sequence.serialize_element(address)?;
        }
        sequence.end()
    }
}

struct FactsProjection<'a> {
    index: &'a ReferenceIndex,
    count: usize,
}

impl Serialize for FactsProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.count))?;
        for fact in &self.index.facts()[..self.count] {
            sequence.serialize_element(&FactProjection {
                index: self.index,
                fact,
            })?;
        }
        sequence.end()
    }
}

struct FactProjection<'a> {
    index: &'a ReferenceIndex,
    fact: &'a ReferenceFact,
}

impl Serialize for FactProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let source = self
            .index
            .address(self.fact.source().object())
            .ok_or_else(|| S::Error::custom("reference fact source has no portable address"))?;
        let mut output = serializer.serialize_struct("ReferenceFact", 5)?;
        output.serialize_field("source", source)?;
        output.serialize_field("field_path", self.fact.field_path())?;
        output.serialize_field("raw_target", &RawTargetProjection(self.fact.raw_target()))?;
        output.serialize_field(
            "resolution",
            &ResolutionProjection {
                index: self.index,
                resolution: self.fact.resolution(),
            },
        )?;
        output.serialize_field("diagnostics", self.fact.diagnostics())?;
        output.end()
    }
}

struct RawTargetProjection<'a>(&'a RawReferenceTarget);

impl Serialize for RawTargetProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0 {
            RawReferenceTarget::Binary {
                file_id,
                path_id,
                external,
            } => {
                let mut output = serializer.serialize_struct("BinaryReferenceTarget", 4)?;
                output.serialize_field("format", "binary")?;
                output.serialize_field("file_id", file_id)?;
                output.serialize_field("path_id", path_id)?;
                output.serialize_field(
                    "external",
                    &external.as_ref().map(BinaryExternalProjection),
                )?;
                output.end()
            }
            RawReferenceTarget::Yaml {
                file_id,
                guid,
                type_id,
            } => {
                let mut output = serializer.serialize_struct("YamlReferenceTarget", 4)?;
                output.serialize_field("format", "yaml")?;
                output.serialize_field("file_id", file_id)?;
                output.serialize_field("guid", &guid.as_ref().map(GuidProjection))?;
                output.serialize_field("type_id", type_id)?;
                output.end()
            }
        }
    }
}

struct BinaryExternalProjection<'a>(&'a super::fact::BinaryExternalReference);

impl Serialize for BinaryExternalProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut output = serializer.serialize_struct("BinaryExternalReference", 4)?;
        output.serialize_field("index", &self.0.index())?;
        output.serialize_field("guid", &self.0.guid())?;
        output.serialize_field("type_id", &self.0.type_id())?;
        output.serialize_field("path", self.0.path())?;
        output.end()
    }
}

struct GuidProjection<'a>(&'a ReferenceGuid);

impl Serialize for GuidProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0 {
            ReferenceGuid::Parsed(guid) => {
                let mut output = serializer.serialize_struct("ParsedGuid", 2)?;
                output.serialize_field("state", "parsed")?;
                output.serialize_field("bytes", guid)?;
                output.end()
            }
            ReferenceGuid::Invalid(value) => {
                let mut output = serializer.serialize_struct("InvalidGuid", 2)?;
                output.serialize_field("state", "invalid")?;
                output.serialize_field("value", value)?;
                output.end()
            }
        }
    }
}

struct ResolutionProjection<'a> {
    index: &'a ReferenceIndex,
    resolution: &'a ReferenceResolution,
}

impl Serialize for ResolutionProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.resolution {
            ReferenceResolution::Null => {
                let mut output = serializer.serialize_struct("NullReference", 1)?;
                output.serialize_field("state", "null")?;
                output.end()
            }
            ReferenceResolution::Resolved(target) => {
                let target = self.index.address(target.object()).ok_or_else(|| {
                    S::Error::custom("resolved reference target has no portable address")
                })?;
                let mut output = serializer.serialize_struct("ResolvedReference", 2)?;
                output.serialize_field("state", "resolved")?;
                output.serialize_field("target", target)?;
                output.end()
            }
            ReferenceResolution::Unloaded { source } => {
                let mut output = serializer.serialize_struct("UnloadedReference", 2)?;
                output.serialize_field("state", "unloaded")?;
                output.serialize_field("source", source)?;
                output.end()
            }
            ReferenceResolution::Missing { target } => {
                let mut output = serializer.serialize_struct("MissingReference", 2)?;
                output.serialize_field("state", "missing")?;
                output.serialize_field("target", target)?;
                output.end()
            }
            ReferenceResolution::Ambiguous { candidates } => {
                let mut output = serializer.serialize_struct("AmbiguousReference", 2)?;
                output.serialize_field("state", "ambiguous")?;
                output.serialize_field("candidates", candidates)?;
                output.end()
            }
            ReferenceResolution::Invalid { diagnostic } => {
                let mut output = serializer.serialize_struct("InvalidReference", 2)?;
                output.serialize_field("state", "invalid")?;
                output.serialize_field("diagnostic", diagnostic)?;
                output.end()
            }
        }
    }
}

struct BudgetWriter<'a, W: Write + ?Sized> {
    inner: &'a mut W,
    budget: &'a mut AssetLoadBudget,
    bytes_written: u64,
    budget_error: Option<BudgetError>,
}

impl<'a, W: Write + ?Sized> BudgetWriter<'a, W> {
    fn new(inner: &'a mut W, budget: &'a mut AssetLoadBudget) -> Self {
        Self {
            inner,
            budget,
            bytes_written: 0,
            budget_error: None,
        }
    }
}

impl<W: Write + ?Sized> Write for BudgetWriter<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let requested = u64::try_from(buffer.len())
            .map_err(|_| io::Error::other("projection write length does not fit u64"))?;
        if let Err(error) = self.budget.check_bytes(requested) {
            self.budget_error = Some(error.clone());
            return Err(io::Error::other(error));
        }
        let written = self.inner.write(buffer)?;
        let written = u64::try_from(written)
            .map_err(|_| io::Error::other("projection write length does not fit u64"))?;
        if let Err(error) = self.budget.consume_bytes(written) {
            self.budget_error = Some(error.clone());
            return Err(io::Error::other(error));
        }
        self.bytes_written = self
            .bytes_written
            .checked_add(written)
            .ok_or_else(|| io::Error::other("projection byte counter overflow"))?;
        usize::try_from(written)
            .map_err(|_| io::Error::other("projection write length does not fit usize"))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn finish_json<W: Write + ?Sized>(
    result: Result<(), serde_json::Error>,
    writer: &mut BudgetWriter<'_, W>,
    format: ReferenceProjectionFormat,
) -> Result<(), ReferenceGraphError> {
    match result {
        Ok(()) => Ok(()),
        Err(_) if writer.budget_error.is_some() => Err(ReferenceGraphError::Budget(
            writer
                .budget_error
                .take()
                .ok_or(ReferenceGraphError::Invariant(
                    "projection budget error disappeared",
                ))?,
        )),
        Err(error) if error.is_io() => {
            let kind = error.io_error_kind().unwrap_or(io::ErrorKind::Other);
            Err(ReferenceGraphError::ProjectionIo {
                format,
                bytes_written: writer.bytes_written,
                source: io::Error::new(kind, error),
            })
        }
        Err(error) => Err(ReferenceGraphError::ProjectionEncoding {
            format,
            source: error,
        }),
    }
}

fn dot_io<W: Write + ?Sized>(
    result: io::Result<()>,
    writer: &mut BudgetWriter<'_, W>,
) -> Result<(), ReferenceGraphError> {
    finish_io(result, writer, ReferenceProjectionFormat::DotV1)
}

fn finish_flush<W: Write + ?Sized>(
    writer: &mut BudgetWriter<'_, W>,
    format: ReferenceProjectionFormat,
) -> Result<(), ReferenceGraphError> {
    finish_io(writer.flush(), writer, format)
}

fn finish_io<W: Write + ?Sized>(
    result: io::Result<()>,
    writer: &mut BudgetWriter<'_, W>,
    format: ReferenceProjectionFormat,
) -> Result<(), ReferenceGraphError> {
    match result {
        Ok(()) => Ok(()),
        Err(_) if writer.budget_error.is_some() => Err(ReferenceGraphError::Budget(
            writer
                .budget_error
                .take()
                .ok_or(ReferenceGraphError::Invariant(
                    "projection budget error disappeared",
                ))?,
        )),
        Err(source) => Err(ReferenceGraphError::ProjectionIo {
            format,
            bytes_written: writer.bytes_written,
            source,
        }),
    }
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, BudgetError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource })
}

fn u64_to_usize(value: u64, resource: &'static str) -> Result<usize, BudgetError> {
    usize::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource })
}

struct ResolutionAnalysis {
    counts: ReferenceResolutionCounts,
    selected_resolved_edges: u64,
    dot_resolved_edges: u64,
}

fn analyze_resolutions(
    index: &ReferenceIndex,
    fact_count: usize,
    node_count: usize,
) -> Result<ResolutionAnalysis, ReferenceGraphError> {
    let mut counts = ReferenceResolutionCounts::default();
    let mut selected_resolved_edges = 0_u64;
    let mut dot_resolved_edges = 0_u64;
    for (ordinal, fact) in index.facts().iter().enumerate() {
        let counter = match fact.resolution() {
            ReferenceResolution::Null => &mut counts.null,
            ReferenceResolution::Resolved(_) => &mut counts.resolved,
            ReferenceResolution::Unloaded { .. } => &mut counts.unloaded,
            ReferenceResolution::Missing { .. } => &mut counts.missing,
            ReferenceResolution::Ambiguous { .. } => &mut counts.ambiguous,
            ReferenceResolution::Invalid { .. } => &mut counts.invalid,
        };
        *counter = counter
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "reference resolution counts",
            })?;

        if ordinal >= fact_count {
            continue;
        }
        let Some(target) = fact.resolution().resolved() else {
            continue;
        };
        selected_resolved_edges =
            selected_resolved_edges
                .checked_add(1)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "selected resolved reference edges",
                })?;
        let source_ordinal =
            index
                .node_ordinal(fact.source().object())
                .ok_or(ReferenceGraphError::Invariant(
                    "reference fact owner is absent from the node index",
                ))?;
        let target_ordinal =
            index
                .node_ordinal(target.object())
                .ok_or(ReferenceGraphError::Invariant(
                    "resolved reference target is absent from the node index",
                ))?;
        if source_ordinal < node_count && target_ordinal < node_count {
            dot_resolved_edges =
                dot_resolved_edges
                    .checked_add(1)
                    .ok_or(BudgetError::ArithmeticOverflow {
                        resource: "DOT resolved reference edges",
                    })?;
        }
    }
    Ok(ResolutionAnalysis {
        counts,
        selected_resolved_edges,
        dot_resolved_edges,
    })
}

#[cfg(test)]
mod tests {
    use unity_asset_core::{
        AssetLoadLimits, Diagnostic, DiagnosticSeverity, DigestV1, FieldPath, ObjectAddress,
        ObjectId, RevisionedObjectHandle, SourceId, SourceKind, SourceLocator,
    };

    use super::*;
    use crate::reference::fact::{RawReferenceTarget, ReferenceResolution};
    use crate::reference::index::ReferenceIndexInput;
    use crate::reference::{ReferenceGraphCoverage, ReferenceGraphError};

    fn graph() -> ReferenceIndex {
        let workspace = WorkspaceId::from_u128(1).expect("workspace identity");
        let revision = WorkspaceRevision::new(DigestV1::hash_bytes(b"projection tests"));
        let source = SourceId::new(workspace, SourceKind::SerializedFile, 1)
            .expect("serialized source identity");
        let source_handle = handle(workspace, revision, source, 1);
        let target_handle = handle(workspace, revision, source, 2);
        let locator = SourceLocator::path("projection.assets").expect("source locator");
        let addresses = vec![
            ObjectAddress::binary_at(locator.clone(), 1).expect("source address"),
            ObjectAddress::binary_at(locator, 2).expect("target address"),
        ];
        let invalid = diagnostic(9);
        let resolutions = [
            ReferenceResolution::Null,
            ReferenceResolution::Resolved(target_handle.clone()),
            ReferenceResolution::Unloaded { source: None },
            ReferenceResolution::Missing { target: None },
            ReferenceResolution::Ambiguous {
                candidates: Vec::new().into_boxed_slice(),
            },
            ReferenceResolution::Invalid {
                diagnostic: invalid,
            },
        ];
        let facts = resolutions
            .into_iter()
            .enumerate()
            .map(|(ordinal, resolution)| {
                ReferenceFact::new(
                    source_handle.clone(),
                    FieldPath::root()
                        .push_field(format!("reference_{ordinal}"))
                        .expect("field path"),
                    RawReferenceTarget::Binary {
                        file_id: 0,
                        path_id: i64::try_from(ordinal + 1).expect("pathID"),
                        external: None,
                    },
                    resolution,
                    Vec::new().into_boxed_slice(),
                )
            })
            .collect::<Vec<_>>();
        let diagnostics = (0..3).map(diagnostic).collect::<Vec<_>>();
        let coverage = ReferenceGraphCoverage::new(1, 1, 2, 2, 6, true, Vec::new());
        ReferenceIndex::build(
            ReferenceIndexInput {
                workspace,
                revision,
                nodes: vec![source_handle, target_handle],
                addresses,
                facts,
                diagnostics,
                coverage,
                source_occurrences: Vec::new(),
            },
            &mut AssetLoadBudget::default(),
        )
        .expect("reference index")
    }

    fn handle(
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        source: SourceId,
        path_id: i64,
    ) -> RevisionedObjectHandle {
        let object = ObjectId::binary(source, path_id).expect("binary object identity");
        RevisionedObjectHandle::new(workspace, revision, object).expect("revisioned object handle")
    }

    fn diagnostic(ordinal: usize) -> Diagnostic {
        Diagnostic::new(
            DiagnosticSeverity::Warning,
            format!("PROJECTION_{ordinal}"),
            format!("projection diagnostic {ordinal}"),
        )
        .expect("diagnostic")
    }

    fn exact_projection_budget() -> AssetLoadBudget {
        AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 9,
            max_members: 6,
            ..AssetLoadLimits::default()
        })
        .expect("projection budget")
    }

    #[test]
    fn every_format_reports_the_same_deterministic_diagnostic_truncation() {
        let graph = graph();
        for format in [
            ReferenceProjectionFormat::JsonV1,
            ReferenceProjectionFormat::JsonLinesV1,
            ReferenceProjectionFormat::DotV1,
        ] {
            let mut output = Vec::new();
            let mut budget = exact_projection_budget();
            let report = write_projection(
                &graph,
                &mut output,
                ReferenceProjectionOptions::new(format).with_max_diagnostics(1),
                &mut budget,
            )
            .expect("projection");

            assert_eq!(budget.usage().entries, 9);
            assert_eq!(budget.usage().members, 6);
            assert_eq!(report.facts_written(), 6);
            assert_eq!(report.resolved_edges_written(), 1);
            assert_eq!(report.diagnostics_written(), 1);
            assert_eq!(report.total_diagnostics(), 3);
            assert!(report.diagnostics_truncated());
            assert!(!report.is_complete());
            assert_eq!(report.resolution_counts().null(), 1);
            assert_eq!(report.resolution_counts().resolved(), 1);
            assert_eq!(report.resolution_counts().unloaded(), 1);
            assert_eq!(report.resolution_counts().missing(), 1);
            assert_eq!(report.resolution_counts().ambiguous(), 1);
            assert_eq!(report.resolution_counts().invalid(), 1);

            assert_projection_diagnostics(format, &output);
        }
    }

    fn assert_projection_diagnostics(format: ReferenceProjectionFormat, output: &[u8]) {
        match format {
            ReferenceProjectionFormat::JsonV1 => {
                let document: serde_json::Value =
                    serde_json::from_slice(output).expect("JSON projection");
                assert_eq!(document["projection"]["diagnostics_written"], 1);
                assert_eq!(document["projection"]["resolved_edges_written"], 1);
                assert_eq!(document["projection"]["total_diagnostics"], 3);
                assert_eq!(document["projection"]["diagnostics_truncated"], true);
                assert_eq!(
                    document["diagnostics"]
                        .as_array()
                        .expect("diagnostics")
                        .len(),
                    1
                );
                assert_eq!(document["diagnostics"][0]["code"], "PROJECTION_0");
            }
            ReferenceProjectionFormat::JsonLinesV1 => {
                let lines = output
                    .split(|byte| *byte == b'\n')
                    .filter(|line| !line.is_empty())
                    .map(|line| serde_json::from_slice::<serde_json::Value>(line).expect("JSONL"))
                    .collect::<Vec<_>>();
                assert_eq!(lines[0]["projection"]["diagnostics_written"], 1);
                assert_eq!(lines[0]["projection"]["resolved_edges_written"], 1);
                assert_eq!(lines[0]["projection"]["total_diagnostics"], 3);
                assert_eq!(lines[0]["projection"]["diagnostics_truncated"], true);
                let diagnostics = lines
                    .iter()
                    .filter(|line| line["kind"] == "diagnostic")
                    .collect::<Vec<_>>();
                assert_eq!(diagnostics.len(), 1);
                assert_eq!(diagnostics[0]["diagnostic"]["code"], "PROJECTION_0");
            }
            ReferenceProjectionFormat::DotV1 => {
                let dot = std::str::from_utf8(output).expect("DOT projection");
                assert!(dot.contains(
                    "facts=6/6 resolved_edges=1 diagnostics=1/3 diagnostics_truncated=true"
                ));
                assert_eq!(dot.matches("  // diagnostic=").count(), 1);
                assert!(dot.contains("\"code\":\"PROJECTION_0\""));
            }
        }
    }

    #[test]
    fn facts_written_is_format_invariant_while_dot_edges_are_explicitly_lossy() {
        let graph = graph();
        for (format, expected_edges) in [
            (ReferenceProjectionFormat::JsonV1, 1),
            (ReferenceProjectionFormat::JsonLinesV1, 1),
            (ReferenceProjectionFormat::DotV1, 0),
        ] {
            let mut output = Vec::new();
            let report = write_projection(
                &graph,
                &mut output,
                ReferenceProjectionOptions::new(format).with_max_nodes(1),
                &mut AssetLoadBudget::default(),
            )
            .expect("projection");
            assert_eq!(report.facts_written(), 6);
            assert_eq!(report.resolved_edges_written(), expected_edges);

            match format {
                ReferenceProjectionFormat::JsonV1 => {
                    let document: serde_json::Value =
                        serde_json::from_slice(&output).expect("JSON projection");
                    assert_eq!(document["projection"]["facts_written"], 6);
                    assert_eq!(document["projection"]["resolved_edges_written"], 1);
                }
                ReferenceProjectionFormat::JsonLinesV1 => {
                    let header: serde_json::Value = serde_json::from_slice(
                        output.split(|byte| *byte == b'\n').next().expect("header"),
                    )
                    .expect("JSONL header");
                    assert_eq!(header["projection"]["facts_written"], 6);
                    assert_eq!(header["projection"]["resolved_edges_written"], 1);
                }
                ReferenceProjectionFormat::DotV1 => {
                    let dot = std::str::from_utf8(&output).expect("DOT projection");
                    assert!(dot.contains("facts=6/6 resolved_edges=0"));
                    assert!(!dot.contains(" -> "));
                }
            }
        }
    }

    #[test]
    fn hard_entry_and_member_limits_fail_before_any_output() {
        let graph = graph();
        let options = ReferenceProjectionOptions::new(ReferenceProjectionFormat::JsonV1)
            .with_max_diagnostics(1);

        let mut entry_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 8,
            max_members: 6,
            ..AssetLoadLimits::default()
        })
        .expect("entry budget");
        let mut output = Vec::new();
        let error = write_projection(&graph, &mut output, options, &mut entry_budget)
            .expect_err("entry budget must fail");
        assert!(matches!(
            error,
            ReferenceGraphError::Budget(BudgetError::Exceeded {
                resource: "entries",
                limit: 8,
                requested: 9,
            })
        ));
        assert!(output.is_empty());
        assert_eq!(entry_budget.usage().entries, 0);
        assert_eq!(entry_budget.usage().members, 0);

        let mut member_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 9,
            max_members: 5,
            ..AssetLoadLimits::default()
        })
        .expect("member budget");
        let error = write_projection(&graph, &mut output, options, &mut member_budget)
            .expect_err("member budget must fail");
        assert!(matches!(
            error,
            ReferenceGraphError::Budget(BudgetError::Exceeded {
                resource: "members",
                limit: 5,
                requested: 6,
            })
        ));
        assert!(output.is_empty());
        assert_eq!(member_budget.usage().entries, 0);
        assert_eq!(member_budget.usage().members, 0);
    }

    #[test]
    fn serde_writer_failures_remain_typed_projection_io_errors() {
        struct FailingWriter;

        impl Write for FailingWriter {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let graph = graph();
        let error = write_projection(
            &graph,
            &mut FailingWriter,
            ReferenceProjectionOptions::new(ReferenceProjectionFormat::JsonV1),
            &mut AssetLoadBudget::default(),
        )
        .expect_err("writer must fail");
        match error {
            ReferenceGraphError::ProjectionIo { format, source, .. } => {
                assert_eq!(format, ReferenceProjectionFormat::JsonV1);
                assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected projection I/O error, got {other:?}"),
        }
    }
}
