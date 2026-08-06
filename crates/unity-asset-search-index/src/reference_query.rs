use std::cmp::Ordering;
use std::collections::{BinaryHeap, TryReserveError};
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tantivy::collector::{Collector, Count, SegmentCollector};
use tantivy::columnar::{Column, StrColumn};
use tantivy::query::TermQuery;
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{DocAddress, DocId, IndexReader, Score, Term};
#[cfg(test)]
use unity_asset_core::BudgetedJsonError;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, Diagnostic, DiagnosticError, DigestV1, FieldPath, FieldPathError,
    FieldPathSegment, ObjectAddress, SourceLocator, YamlFileId, arc_slice_allocation_bytes,
    string_allocation_bytes, vec_allocation_bytes,
};
use unity_asset_search_protocol::{
    ApiErrorCode, GenerationIdV1, MAX_REFERENCE_RESULTS, QueryPolicyId, ReferenceContext,
    ReferenceCoverage, ReferenceCursor, ReferenceDiagnosticCoverage, ReferenceDirection,
    ReferenceHit, ReferenceObject, ReferenceRequest, ReferenceSelector, ReferencesResponse,
    SEARCH_PROTOCOL_REVISION, WireProjectionError,
};

use crate::analysis::{GuidProjection, RawReferenceProjection, ReferenceResolutionProjection};
use crate::generation::{GenerationStamp, GenerationStorageContract};
#[cfg(test)]
use crate::projection::reference_object_key;
use crate::projection::{reference_guid_key, reference_object_key_for};
use crate::reference_payload::{
    MAX_REFERENCE_PAYLOAD_BYTES, ReferencePayload, ReferencePayloadReadError,
    ReferencePayloadReader,
};
use crate::store::{ReferenceProjectionFields, ReferenceProjectionReader};
use crate::wire;

pub(crate) const MAX_REFERENCE_QUERY_LIMIT: usize = MAX_REFERENCE_RESULTS as usize;
const MAX_REFERENCE_CURSOR_STABLE_ID_BYTES: usize = 256;
const MAX_REFERENCE_PAGE_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_REFERENCE_OBJECTS_PER_HIT: usize = 1024;
const MAX_REFERENCE_HIT_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_REFERENCE_PAGE_HIT_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_REFERENCE_RESPONSE_DIAGNOSTICS: usize = 128;
const MAX_REFERENCE_RESPONSE_DIAGNOSTIC_JSON_BYTES: usize = 256 * 1024;
const REFERENCE_CURSOR_BINDING_PREFIX: &str = "reference-query-v2:";

#[derive(Debug, Clone, Copy)]
struct ReferenceQueryLimits {
    max_payload_bytes: usize,
    max_page_payload_bytes: usize,
    max_hit_json_bytes: usize,
    max_page_hit_json_bytes: usize,
    max_response_diagnostics: usize,
    max_response_diagnostic_json_bytes: usize,
}

const REFERENCE_QUERY_LIMITS: ReferenceQueryLimits = ReferenceQueryLimits {
    max_payload_bytes: MAX_REFERENCE_PAYLOAD_BYTES,
    max_page_payload_bytes: MAX_REFERENCE_PAGE_PAYLOAD_BYTES,
    max_hit_json_bytes: MAX_REFERENCE_HIT_JSON_BYTES,
    max_page_hit_json_bytes: MAX_REFERENCE_PAGE_HIT_JSON_BYTES,
    max_response_diagnostics: MAX_REFERENCE_RESPONSE_DIAGNOSTICS,
    max_response_diagnostic_json_bytes: MAX_REFERENCE_RESPONSE_DIAGNOSTIC_JSON_BYTES,
};

/// Completeness evidence captured with the immutable reference projection.
///
/// A query can report an exact total only when both the analysis and projection passes were
/// complete. Diagnostics are generation-bound and therefore cannot drift while a page is read.
/// Their retained projection is bounded before the generation becomes queryable.
#[derive(Debug, Clone)]
pub(crate) struct ReferenceQueryCompleteness {
    analysis_complete: bool,
    projection_complete: bool,
    diagnostics: Option<Arc<[Diagnostic]>>,
    diagnostics_truncated: bool,
    diagnostic_json_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReferenceQueryAllocationUnit {
    Bytes,
    Elements,
}

impl fmt::Display for ReferenceQueryAllocationUnit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Bytes => "bytes",
            Self::Elements => "elements",
        })
    }
}

#[derive(Debug)]
pub(crate) enum ReferenceQueryCompletenessError {
    Budget(BudgetError),
    Allocation {
        resource: &'static str,
        requested: usize,
        unit: ReferenceQueryAllocationUnit,
        source: TryReserveError,
    },
    Diagnostic(DiagnosticError),
    FieldPath(FieldPathError),
    Serialization(serde_json::Error),
}

impl fmt::Display for ReferenceQueryCompletenessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Budget(error) => fmt::Display::fmt(error, formatter),
            Self::Allocation {
                resource,
                requested,
                unit,
                source,
            } => write!(
                formatter,
                "failed to reserve {requested} {unit} for {resource}: {source}"
            ),
            Self::Diagnostic(error) => fmt::Display::fmt(error, formatter),
            Self::FieldPath(error) => fmt::Display::fmt(error, formatter),
            Self::Serialization(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl Error for ReferenceQueryCompletenessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Budget(error) => Some(error),
            Self::Allocation { source, .. } => Some(source),
            Self::Diagnostic(error) => Some(error),
            Self::FieldPath(error) => Some(error),
            Self::Serialization(error) => Some(error),
        }
    }
}

impl From<BudgetError> for ReferenceQueryCompletenessError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<DiagnosticError> for ReferenceQueryCompletenessError {
    fn from(error: DiagnosticError) -> Self {
        Self::Diagnostic(error)
    }
}

impl From<FieldPathError> for ReferenceQueryCompletenessError {
    fn from(error: FieldPathError) -> Self {
        Self::FieldPath(error)
    }
}

struct DiagnosticProjection<'diagnostic> {
    diagnostics: Vec<&'diagnostic Diagnostic>,
    json_bytes: usize,
    truncated: bool,
}

impl ReferenceQueryCompleteness {
    pub(crate) fn new<'diagnostic>(
        analysis_complete: bool,
        projection_complete: bool,
        diagnostics: impl IntoIterator<Item = &'diagnostic Diagnostic>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ReferenceQueryCompletenessError> {
        let DiagnosticProjection {
            mut diagnostics,
            json_bytes,
            truncated,
        } = collect_diagnostic_references(diagnostics, budget)?;
        diagnostics.sort_unstable();

        if diagnostics.is_empty() {
            return Ok(Self {
                analysis_complete,
                projection_complete,
                diagnostics: None,
                diagnostics_truncated: truncated,
                diagnostic_json_bytes: 2,
            });
        }

        let retained_members = usize_to_budget_count(diagnostics.len(), "members")?;
        let retained_bytes = retained_diagnostic_bytes(&diagnostics)?;
        budget.check_members(retained_members)?;
        budget.check_bytes(retained_bytes)?;
        budget.consume_members(retained_members)?;
        budget.consume_bytes(retained_bytes)?;

        let mut retained = Vec::new();
        retained
            .try_reserve_exact(diagnostics.len())
            .map_err(|source| ReferenceQueryCompletenessError::Allocation {
                resource: "reference query completeness diagnostic vector",
                requested: diagnostics.len(),
                unit: ReferenceQueryAllocationUnit::Elements,
                source,
            })?;
        for diagnostic in diagnostics {
            retained.push(clone_diagnostic(diagnostic)?);
        }

        Ok(Self {
            analysis_complete,
            projection_complete,
            diagnostics: Some(Arc::from(retained.into_boxed_slice())),
            diagnostics_truncated: truncated,
            diagnostic_json_bytes: json_bytes,
        })
    }

    #[cfg(test)]
    pub(crate) const fn complete() -> Self {
        Self {
            analysis_complete: true,
            projection_complete: true,
            diagnostics: None,
            diagnostics_truncated: false,
            diagnostic_json_bytes: 2,
        }
    }

    const fn is_complete(&self) -> bool {
        self.analysis_complete && self.projection_complete
    }

    fn diagnostics(&self) -> &[Diagnostic] {
        self.diagnostics.as_deref().unwrap_or_default()
    }

    const fn diagnostics_truncated(&self) -> bool {
        self.diagnostics_truncated
    }

    const fn diagnostic_json_bytes(&self) -> usize {
        self.diagnostic_json_bytes
    }
}

fn collect_diagnostic_references<'diagnostic>(
    diagnostics: impl IntoIterator<Item = &'diagnostic Diagnostic>,
    budget: &mut AssetLoadBudget,
) -> Result<DiagnosticProjection<'diagnostic>, ReferenceQueryCompletenessError> {
    let diagnostics = diagnostics.into_iter();
    let initial_capacity = diagnostics
        .size_hint()
        .0
        .min(MAX_REFERENCE_RESPONSE_DIAGNOSTICS);
    budget.check_entries(usize_to_budget_count(initial_capacity, "entries")?)?;

    let mut retained = Vec::new();
    reserve_diagnostic_references(&mut retained, initial_capacity, budget)?;
    let mut json_bytes = 2;
    let mut truncated = false;
    for diagnostic in diagnostics {
        budget.check_entries(1)?;
        budget.consume_entries(1)?;
        if retained.contains(&diagnostic) {
            continue;
        }
        if retained.len() >= MAX_REFERENCE_RESPONSE_DIAGNOSTICS {
            truncated = true;
            break;
        }
        let encoded_bytes =
            json_bytes_for(diagnostic).map_err(ReferenceQueryCompletenessError::Serialization)?;
        let separator_bytes = usize::from(!retained.is_empty());
        let Some(framed_bytes) = encoded_bytes.checked_add(separator_bytes) else {
            truncated = true;
            break;
        };
        if framed_bytes > MAX_REFERENCE_RESPONSE_DIAGNOSTIC_JSON_BYTES.saturating_sub(json_bytes) {
            truncated = true;
            break;
        }
        if retained.len() == retained.capacity() {
            let required =
                retained
                    .len()
                    .checked_add(1)
                    .ok_or(BudgetError::ArithmeticOverflow {
                        resource: "entries",
                    })?;
            let target = if retained.capacity() == 0 {
                required
            } else {
                retained
                    .capacity()
                    .checked_mul(2)
                    .ok_or(BudgetError::ArithmeticOverflow {
                        resource: "entries",
                    })?
                    .max(required)
                    .min(MAX_REFERENCE_RESPONSE_DIAGNOSTICS)
            };
            reserve_diagnostic_references(&mut retained, target, budget)?;
        }
        retained.push(diagnostic);
        json_bytes += framed_bytes;
    }
    Ok(DiagnosticProjection {
        diagnostics: retained,
        json_bytes,
        truncated,
    })
}

fn reserve_diagnostic_references(
    diagnostics: &mut Vec<&Diagnostic>,
    target_capacity: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), ReferenceQueryCompletenessError> {
    let previous_capacity = diagnostics.capacity();
    if target_capacity <= previous_capacity {
        return Ok(());
    }

    let planned_growth = target_capacity
        .checked_sub(previous_capacity)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let planned_bytes = checked_vec_bytes::<&Diagnostic>(planned_growth)?;
    budget.check_bytes(planned_bytes)?;
    budget.consume_bytes(planned_bytes)?;

    let requested =
        target_capacity
            .checked_sub(diagnostics.len())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "entries",
            })?;
    diagnostics.try_reserve_exact(requested).map_err(|source| {
        ReferenceQueryCompletenessError::Allocation {
            resource: "reference query completeness diagnostic references",
            requested,
            unit: ReferenceQueryAllocationUnit::Elements,
            source,
        }
    })
}

fn retained_diagnostic_bytes(
    diagnostics: &[&Diagnostic],
) -> Result<u64, ReferenceQueryCompletenessError> {
    let mut retained = checked_vec_bytes::<Diagnostic>(diagnostics.len())?
        .checked_add(checked_arc_slice_bytes::<Diagnostic>(diagnostics.len())?)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    for diagnostic in diagnostics {
        let code_bytes = checked_string_bytes(diagnostic.code().len())?;
        let message_bytes = checked_string_bytes(diagnostic.message().len())?;
        retained = retained
            .checked_add(code_bytes)
            .and_then(|bytes| bytes.checked_add(message_bytes))
            .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
        if let Some(address) = diagnostic.address() {
            retained = retained
                .checked_add(usize_to_budget_bytes(
                    address
                        .retained_clone_bytes()
                        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?,
                )?)
                .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
        }
        if let Some(field_path) = diagnostic.field_path() {
            retained = retained
                .checked_add(usize_to_budget_bytes(
                    field_path
                        .retained_clone_bytes()
                        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?,
                )?)
                .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
        }
    }
    Ok(retained)
}

fn clone_diagnostic(
    diagnostic: &Diagnostic,
) -> Result<Diagnostic, ReferenceQueryCompletenessError> {
    let code = clone_string(diagnostic.code(), "reference query diagnostic code")?;
    let message = clone_string(diagnostic.message(), "reference query diagnostic message")?;
    let mut retained = Diagnostic::new(diagnostic.severity(), code, message)?;
    if let Some(address) = diagnostic.address() {
        // ObjectAddress exposes retained-clone accounting but no public fallible clone builder.
        retained = retained.at_address(address.clone());
    }
    if let Some(field_path) = diagnostic.field_path() {
        retained = retained.at_field(clone_field_path(field_path)?);
    }
    Ok(retained)
}

fn clone_field_path(field_path: &FieldPath) -> Result<FieldPath, ReferenceQueryCompletenessError> {
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(field_path.segments().len())
        .map_err(|source| ReferenceQueryCompletenessError::Allocation {
            resource: "reference query diagnostic field path",
            requested: field_path.segments().len(),
            unit: ReferenceQueryAllocationUnit::Elements,
            source,
        })?;
    for segment in field_path.segments() {
        segments.push(match segment {
            FieldPathSegment::Field(name) => FieldPathSegment::field(clone_string(
                name,
                "reference query diagnostic field name",
            )?)?,
            FieldPathSegment::Index(index) => FieldPathSegment::Index(*index),
        });
    }
    Ok(FieldPath::from_segments(segments)?)
}

fn clone_string(
    value: &str,
    resource: &'static str,
) -> Result<String, ReferenceQueryCompletenessError> {
    let mut retained = String::new();
    retained.try_reserve_exact(value.len()).map_err(|source| {
        ReferenceQueryCompletenessError::Allocation {
            resource,
            requested: value.len(),
            unit: ReferenceQueryAllocationUnit::Bytes,
            source,
        }
    })?;
    retained.push_str(value);
    Ok(retained)
}

fn checked_vec_bytes<T>(capacity: usize) -> Result<u64, ReferenceQueryCompletenessError> {
    vec_allocation_bytes::<T>(capacity)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" }.into())
}

fn checked_string_bytes(capacity: usize) -> Result<u64, ReferenceQueryCompletenessError> {
    string_allocation_bytes(capacity)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" }.into())
}

fn checked_arc_slice_bytes<T>(length: usize) -> Result<u64, ReferenceQueryCompletenessError> {
    arc_slice_allocation_bytes::<T>(length)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" }.into())
}

fn usize_to_budget_bytes(value: usize) -> Result<u64, ReferenceQueryCompletenessError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" }.into())
}

fn usize_to_budget_count(
    value: usize,
    resource: &'static str,
) -> Result<u64, ReferenceQueryCompletenessError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource }.into())
}

/// One immutable reference reader and the generation identity that owns it.
#[derive(Clone)]
pub(crate) struct ReferenceQuerySnapshot {
    generation: GenerationStamp,
    reader: IndexReader,
    fields: ReferenceProjectionFields,
    payloads: ReferencePayloadReader,
    storage: GenerationStorageContract,
    completeness: ReferenceQueryCompleteness,
}

impl ReferenceQuerySnapshot {
    pub(crate) fn new(
        generation: GenerationStamp,
        projection: &ReferenceProjectionReader,
        completeness: ReferenceQueryCompleteness,
    ) -> Self {
        Self {
            generation,
            reader: projection.reader().clone(),
            fields: *projection.fields(),
            payloads: projection.payloads().clone(),
            storage: projection.storage(),
            completeness,
        }
    }
}

struct ResponseDiagnostics {
    values: Vec<Diagnostic>,
    total: Option<usize>,
    truncated: bool,
    serialized_bytes: usize,
    max_count: usize,
    max_serialized_bytes: usize,
}

impl ResponseDiagnostics {
    fn new(
        completeness: &ReferenceQueryCompleteness,
        limits: ReferenceQueryLimits,
    ) -> Result<Self, ReferenceQueryError> {
        let mut response = Self {
            values: Vec::new(),
            total: (!completeness.diagnostics_truncated()).then_some(0),
            truncated: completeness.diagnostics_truncated(),
            serialized_bytes: 2,
            max_count: limits.max_response_diagnostics,
            max_serialized_bytes: limits.max_response_diagnostic_json_bytes,
        };
        if limits.max_response_diagnostics == MAX_REFERENCE_RESPONSE_DIAGNOSTICS
            && limits.max_response_diagnostic_json_bytes
                == MAX_REFERENCE_RESPONSE_DIAGNOSTIC_JSON_BYTES
        {
            response.values = Vec::from(completeness.diagnostics());
            response.serialized_bytes = completeness.diagnostic_json_bytes();
            if response.total.is_some() {
                response.total = Some(response.values.len());
            }
        } else {
            response.extend_borrowed(completeness.diagnostics())?;
        }
        Ok(response)
    }

    fn extend_borrowed(&mut self, diagnostics: &[Diagnostic]) -> Result<(), ReferenceQueryError> {
        for diagnostic in diagnostics {
            self.push(diagnostic.clone())?;
        }
        Ok(())
    }

    fn extend_owned(&mut self, diagnostics: Vec<Diagnostic>) -> Result<(), ReferenceQueryError> {
        for diagnostic in diagnostics {
            self.push(diagnostic)?;
        }
        Ok(())
    }

    fn push(&mut self, diagnostic: Diagnostic) -> Result<(), ReferenceQueryError> {
        if let Some(total) = self.total {
            self.total = total.checked_add(1);
            if self.total.is_none() {
                self.truncated = true;
            }
        }

        if self.values.len() >= self.max_count {
            self.truncated = true;
            return Ok(());
        }

        let encoded_bytes = json_bytes_for(&diagnostic).map_err(|error| {
            ReferenceQueryError::ResponseDiagnostic {
                reason: error.to_string(),
            }
        })?;
        let separator_bytes = usize::from(!self.values.is_empty());
        let Some(framed_bytes) = encoded_bytes.checked_add(separator_bytes) else {
            self.truncated = true;
            return Ok(());
        };
        if framed_bytes
            > self
                .max_serialized_bytes
                .saturating_sub(self.serialized_bytes)
        {
            self.truncated = true;
            return Ok(());
        }

        self.serialized_bytes += framed_bytes;
        self.values.push(diagnostic);
        Ok(())
    }

    fn finish(self) -> Result<(Vec<Diagnostic>, ReferenceDiagnosticCoverage), ReferenceQueryError> {
        let coverage = ReferenceDiagnosticCoverage {
            returned: wire::fixed_u32(self.values.len(), "reference response diagnostic count")?,
            truncated: self.truncated,
            total: self
                .total
                .map(|value| wire::fixed_u64(value, "reference response diagnostic total"))
                .transpose()?,
            serialized_bytes: wire::fixed_u64(
                self.serialized_bytes,
                "reference response diagnostic bytes",
            )?,
            max_count: wire::fixed_u32(
                self.max_count,
                "reference response diagnostic count limit",
            )?,
            max_serialized_bytes: wire::fixed_u64(
                self.max_serialized_bytes,
                "reference response diagnostic byte limit",
            )?,
        };
        Ok((self.values, coverage))
    }
}

#[derive(Clone)]
pub(crate) struct ReferenceQueryEngine {
    snapshot: Arc<ReferenceQuerySnapshot>,
}

impl ReferenceQueryEngine {
    pub(crate) fn new(snapshot: Arc<ReferenceQuerySnapshot>) -> Self {
        Self { snapshot }
    }

    pub(crate) fn references(
        &self,
        request: ReferenceRequest,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferencesResponse, ReferenceQueryError> {
        self.references_with_limits(request, REFERENCE_QUERY_LIMITS, budget)
    }

    fn references_with_limits(
        &self,
        request: ReferenceRequest,
        limits: ReferenceQueryLimits,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferencesResponse, ReferenceQueryError> {
        let started = Instant::now();
        validate_request(&request, &self.snapshot.generation)?;
        let request_limit =
            usize::try_from(request.limit).map_err(|_| WireProjectionError::NumericOverflow {
                field: "reference request limit",
            })?;
        let (field, key) = selector_key(
            &request.selector,
            request.direction,
            self.snapshot.fields,
            self.snapshot.storage,
        )?;
        let query_binding = request
            .cursor_query_binding()
            .map_err(|_| ReferenceQueryError::InvalidCursorQueryBinding)?;
        validate_cursor_query_binding(request.cursor.as_ref(), &query_binding)?;

        let searcher = self.snapshot.reader.searcher();
        let query = TermQuery::new(Term::from_field_text(field, &key), IndexRecordOption::Basic);
        let fetch_limit =
            request_limit
                .checked_add(1)
                .ok_or(WireProjectionError::NumericOverflow {
                    field: "reference fetch limit",
                })?;
        let after_stable_id = clone_cursor_stable_id(request.cursor.as_ref(), budget)?;
        let (total, mut documents) = searcher.search(
            &query,
            &(
                Count,
                StableReferenceCollector {
                    limit: fetch_limit,
                    after_stable_id,
                },
            ),
        )?;

        for duplicate in documents.windows(2) {
            if duplicate[0].stable_id == duplicate[1].stable_id {
                return Err(ReferenceQueryError::CorruptDocument {
                    stable_id: Some(duplicate[0].stable_id.clone()),
                    reason: "multiple reference facts share one stable ID".to_owned(),
                });
            }
        }

        let request_limit_has_more = documents.len() > request_limit;
        if request_limit_has_more {
            documents.truncate(request_limit);
        }

        let mut hits = Vec::with_capacity(documents.len());
        let mut diagnostics = ResponseDiagnostics::new(&self.snapshot.completeness, limits)?;
        let mut payload_bytes = 0;
        let mut hit_json_bytes = 2;
        let mut byte_limit_has_more = false;
        let mut last_returned_stable_id = None;
        for selected in &documents {
            let remaining_payload_bytes =
                limits.max_page_payload_bytes.saturating_sub(payload_bytes);
            let Some(decoded) = decode_reference_payload(
                &self.snapshot.payloads,
                selected,
                limits.max_payload_bytes,
                remaining_payload_bytes,
                budget,
            )?
            else {
                byte_limit_has_more = true;
                break;
            };
            let mut stored = decoded.document;

            let fact_diagnostics = std::mem::take(&mut stored.fact.diagnostics);
            let hit = reference_hit(stored, self.snapshot.storage)?;
            let encoded_hit_bytes = reference_hit_json_bytes(&hit)?;
            if encoded_hit_bytes > limits.max_hit_json_bytes {
                return Err(ReferenceQueryError::ResponseHitTooLarge {
                    stable_id: selected.stable_id.clone(),
                    actual: encoded_hit_bytes,
                    maximum: limits.max_hit_json_bytes,
                });
            }
            let framed_hit_bytes = encoded_hit_bytes.saturating_add(usize::from(!hits.is_empty()));
            if framed_hit_bytes
                > limits
                    .max_page_hit_json_bytes
                    .saturating_sub(hit_json_bytes)
            {
                byte_limit_has_more = true;
                break;
            }

            payload_bytes += decoded.payload_bytes;
            hit_json_bytes += framed_hit_bytes;
            diagnostics.extend_owned(fact_diagnostics)?;
            last_returned_stable_id = Some(selected.stable_id.clone());
            hits.push(hit);
        }

        if byte_limit_has_more && last_returned_stable_id.is_none() {
            return Err(ReferenceQueryError::ResponsePageBudgetTooSmall {
                stable_id: documents
                    .first()
                    .map(|document| document.stable_id.clone())
                    .unwrap_or_default(),
                payload_maximum: limits.max_page_payload_bytes,
                hit_json_maximum: limits.max_page_hit_json_bytes,
            });
        }

        let has_more = request_limit_has_more || byte_limit_has_more;
        let complete = self.snapshot.completeness.is_complete();
        let next_cursor = if has_more {
            last_returned_stable_id.map(|after_stable_id| ReferenceCursor {
                generation: wire::generation_id(self.snapshot.generation.generation),
                query_policy_id: wire::query_policy_id(),
                after_stable_id,
                query_binding,
            })
        } else {
            None
        };

        let (diagnostics, diagnostic_coverage) = diagnostics.finish()?;

        Ok(ReferencesResponse {
            protocol_revision: SEARCH_PROTOCOL_REVISION,
            generation: wire::generation_stamp(&self.snapshot.generation),
            query_policy_id: wire::query_policy_id(),
            request,
            took_ms: wire::fixed_millis(
                started.elapsed().as_millis(),
                "reference response duration",
            )?,
            coverage: ReferenceCoverage {
                complete,
                truncated: has_more || !complete,
                returned: wire::fixed_u32(hits.len(), "reference response hit count")?,
                total: complete
                    .then(|| wire::fixed_u64(total, "reference response total"))
                    .transpose()?,
                next_cursor,
            },
            hits,
            diagnostics,
            diagnostic_coverage,
        })
    }
}

#[derive(Debug)]
pub(crate) enum ReferenceQueryError {
    InvalidLimit {
        actual: usize,
        maximum: usize,
    },
    EmptyGuid,
    InvalidGuid,
    EmptyCursor,
    CursorStableIdTooLong {
        actual: usize,
        maximum: usize,
    },
    CursorGenerationMismatch {
        cursor: GenerationIdV1,
        active: GenerationIdV1,
    },
    CursorQueryPolicyMismatch {
        cursor: QueryPolicyId,
        active: QueryPolicyId,
    },
    InvalidCursorQueryBinding,
    CursorQueryMismatch,
    Budget(BudgetError),
    Index(tantivy::TantivyError),
    CorruptDocument {
        stable_id: Option<String>,
        reason: String,
    },
    Payload {
        stable_id: String,
        source: ReferencePayloadReadError,
    },
    ResponseHitTooLarge {
        stable_id: String,
        actual: usize,
        maximum: usize,
    },
    ResponseHitObjectLimitExceeded {
        stable_id: String,
        actual: usize,
        maximum: usize,
    },
    ResponsePageBudgetTooSmall {
        stable_id: String,
        payload_maximum: usize,
        hit_json_maximum: usize,
    },
    ResponseDiagnostic {
        reason: String,
    },
    WireProjection(WireProjectionError),
}

impl ReferenceQueryError {
    pub(crate) const fn api_code(&self) -> ApiErrorCode {
        match self {
            Self::InvalidLimit { .. } | Self::EmptyGuid | Self::InvalidGuid => {
                ApiErrorCode::InvalidRequest
            }
            Self::CursorGenerationMismatch { .. } | Self::CursorQueryPolicyMismatch { .. } => {
                ApiErrorCode::StaleCursor
            }
            Self::EmptyCursor
            | Self::CursorStableIdTooLong { .. }
            | Self::InvalidCursorQueryBinding
            | Self::CursorQueryMismatch => ApiErrorCode::InvalidCursor,
            Self::Budget(_)
            | Self::Index(_)
            | Self::CorruptDocument { .. }
            | Self::Payload { .. }
            | Self::ResponseHitTooLarge { .. }
            | Self::ResponseHitObjectLimitExceeded { .. }
            | Self::ResponsePageBudgetTooSmall { .. }
            | Self::ResponseDiagnostic { .. }
            | Self::WireProjection(_) => ApiErrorCode::Internal,
        }
    }
}

impl fmt::Display for ReferenceQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimit { actual, maximum } => write!(
                formatter,
                "reference query limit {actual} is outside 1..={maximum}"
            ),
            Self::EmptyGuid => formatter.write_str("reference GUID must not be empty"),
            Self::InvalidGuid => {
                formatter.write_str("reference GUID must contain exactly 32 hexadecimal digits")
            }
            Self::EmptyCursor => {
                formatter.write_str("reference cursor stable ID must not be empty")
            }
            Self::CursorStableIdTooLong { actual, maximum } => write!(
                formatter,
                "reference cursor stable ID is {actual} bytes, exceeding the {maximum}-byte limit"
            ),
            Self::CursorGenerationMismatch { cursor, active } => write!(
                formatter,
                "reference cursor generation {cursor} does not match active generation {active}"
            ),
            Self::CursorQueryPolicyMismatch { cursor, active } => write!(
                formatter,
                "reference cursor query policy {cursor} does not match active policy {active}"
            ),
            Self::InvalidCursorQueryBinding => {
                formatter.write_str("reference cursor query binding is malformed")
            }
            Self::CursorQueryMismatch => formatter.write_str(
                "reference cursor belongs to a different selector or reference direction",
            ),
            Self::Budget(error) => fmt::Display::fmt(error, formatter),
            Self::Index(error) => write!(formatter, "reference index query failed: {error}"),
            Self::CorruptDocument { stable_id, reason } => {
                if let Some(stable_id) = stable_id {
                    write!(
                        formatter,
                        "reference projection document {stable_id:?} is corrupt: {reason}"
                    )
                } else {
                    write!(
                        formatter,
                        "reference projection document is corrupt: {reason}"
                    )
                }
            }
            Self::Payload { stable_id, source } => write!(
                formatter,
                "reference projection payload for document {stable_id:?} cannot be decoded: \
                 {source}"
            ),
            Self::ResponseHitTooLarge {
                stable_id,
                actual,
                maximum,
            } => write!(
                formatter,
                "reference hit {stable_id:?} serializes to {actual} bytes, exceeding the \
                 {maximum}-byte per-hit limit"
            ),
            Self::ResponseHitObjectLimitExceeded {
                stable_id,
                actual,
                maximum,
            } => write!(
                formatter,
                "reference hit {stable_id:?} contains {actual} objects, exceeding the \
                 {maximum}-object per-hit limit"
            ),
            Self::ResponsePageBudgetTooSmall {
                stable_id,
                payload_maximum,
                hit_json_maximum,
            } => write!(
                formatter,
                "reference hit {stable_id:?} cannot fit within the page budgets of \
                 {payload_maximum} payload bytes and {hit_json_maximum} response-hit bytes"
            ),
            Self::ResponseDiagnostic { reason } => write!(
                formatter,
                "reference response diagnostic cannot be serialized for response budgeting: {reason}"
            ),
            Self::WireProjection(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl Error for ReferenceQueryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Budget(error) => Some(error),
            Self::Index(error) => Some(error),
            Self::Payload { source, .. } => Some(source),
            Self::WireProjection(error) => Some(error),
            _ => None,
        }
    }
}

impl From<tantivy::TantivyError> for ReferenceQueryError {
    fn from(error: tantivy::TantivyError) -> Self {
        Self::Index(error)
    }
}

impl From<BudgetError> for ReferenceQueryError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<WireProjectionError> for ReferenceQueryError {
    fn from(error: WireProjectionError) -> Self {
        Self::WireProjection(error)
    }
}

fn validate_request(
    request: &ReferenceRequest,
    generation: &GenerationStamp,
) -> Result<(), ReferenceQueryError> {
    let request_limit =
        usize::try_from(request.limit).map_err(|_| WireProjectionError::NumericOverflow {
            field: "reference request limit",
        })?;
    if !(1..=MAX_REFERENCE_QUERY_LIMIT).contains(&request_limit) {
        return Err(ReferenceQueryError::InvalidLimit {
            actual: request_limit,
            maximum: MAX_REFERENCE_QUERY_LIMIT,
        });
    }
    if let Some(cursor) = &request.cursor {
        if cursor.after_stable_id.is_empty() {
            return Err(ReferenceQueryError::EmptyCursor);
        }
        if cursor.after_stable_id.len() > MAX_REFERENCE_CURSOR_STABLE_ID_BYTES {
            return Err(ReferenceQueryError::CursorStableIdTooLong {
                actual: cursor.after_stable_id.len(),
                maximum: MAX_REFERENCE_CURSOR_STABLE_ID_BYTES,
            });
        }
        let active_generation = wire::generation_id(generation.generation);
        if cursor.generation != active_generation {
            return Err(ReferenceQueryError::CursorGenerationMismatch {
                cursor: cursor.generation,
                active: active_generation,
            });
        }
        let active_policy = wire::query_policy_id();
        if cursor.query_policy_id != active_policy {
            return Err(ReferenceQueryError::CursorQueryPolicyMismatch {
                cursor: cursor.query_policy_id,
                active: active_policy,
            });
        }
    }
    Ok(())
}

fn validate_cursor_query_binding(
    cursor: Option<&ReferenceCursor>,
    expected: &str,
) -> Result<(), ReferenceQueryError> {
    let Some(cursor) = cursor else {
        return Ok(());
    };
    let actual = cursor.query_binding.as_str();
    if !is_valid_cursor_query_binding(actual) {
        return Err(ReferenceQueryError::InvalidCursorQueryBinding);
    }
    if actual != expected {
        return Err(ReferenceQueryError::CursorQueryMismatch);
    }
    Ok(())
}

fn clone_cursor_stable_id(
    cursor: Option<&ReferenceCursor>,
    budget: &mut AssetLoadBudget,
) -> Result<Option<Arc<str>>, ReferenceQueryError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let retained_bytes = arc_slice_allocation_bytes::<u8>(cursor.after_stable_id.len())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(Some(Arc::from(cursor.after_stable_id.as_str())))
}

fn is_valid_cursor_query_binding(value: &str) -> bool {
    value
        .strip_prefix(REFERENCE_CURSOR_BINDING_PREFIX)
        .is_some_and(|encoded| {
            encoded.len() == DigestV1::BYTE_LEN * 2
                && encoded
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
}

fn selector_key(
    selector: &ReferenceSelector,
    direction: ReferenceDirection,
    fields: ReferenceProjectionFields,
    storage: GenerationStorageContract,
) -> Result<(Field, String), ReferenceQueryError> {
    let field = match direction {
        ReferenceDirection::Incoming => fields.incoming_key(),
        ReferenceDirection::Outgoing => fields.outgoing_key(),
    };
    let key = match selector {
        ReferenceSelector::Object { address } => reference_object_key_for(storage, address),
        ReferenceSelector::Guid { guid, file_id } => {
            if guid.is_empty() {
                return Err(ReferenceQueryError::EmptyGuid);
            }
            if guid.len() != 32
                || !guid
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(ReferenceQueryError::InvalidGuid);
            }
            reference_guid_key(guid, *file_id)
        }
    };
    Ok((field, key))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StableReferenceDocument {
    stable_id: String,
    address: DocAddress,
    payload_offset: u64,
    payload_length: u64,
    payload_digest: DigestV1,
}

impl Ord for StableReferenceDocument {
    fn cmp(&self, other: &Self) -> Ordering {
        self.stable_id
            .cmp(&other.stable_id)
            .then_with(|| compare_doc_address(self.address, other.address))
    }
}

impl PartialOrd for StableReferenceDocument {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_doc_address(left: DocAddress, right: DocAddress) -> Ordering {
    left.segment_ord
        .cmp(&right.segment_ord)
        .then_with(|| left.doc_id.cmp(&right.doc_id))
}

struct StableReferenceCollector {
    limit: usize,
    after_stable_id: Option<Arc<str>>,
}

impl Collector for StableReferenceCollector {
    type Fruit = Vec<StableReferenceDocument>;
    type Child = StableReferenceSegmentCollector;

    fn for_segment(
        &self,
        segment_local_id: u32,
        segment_reader: &tantivy::SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let stable_ids = segment_reader
            .fast_fields()
            .str("stable_id")?
            .ok_or_else(|| {
                tantivy::TantivyError::SchemaError(
                    "reference stable_id fast field is missing".into(),
                )
            })?;
        let payload_offsets = segment_reader.fast_fields().u64("payload_offset")?;
        let payload_lengths = segment_reader.fast_fields().u64("payload_length")?;
        let payload_digests = segment_reader
            .fast_fields()
            .str("payload_digest")?
            .ok_or_else(|| {
                tantivy::TantivyError::SchemaError(
                    "reference payload_digest fast field is missing".into(),
                )
            })?;
        Ok(StableReferenceSegmentCollector {
            limit: self.limit,
            segment_ord: segment_local_id,
            stable_ids,
            payload_offsets,
            payload_lengths,
            payload_digests,
            after_stable_id: self.after_stable_id.clone(),
            scratch: String::new(),
            digest_scratch: String::new(),
            heap: BinaryHeap::with_capacity(self.limit),
            error: None,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        child_fruits: Vec<<Self::Child as SegmentCollector>::Fruit>,
    ) -> tantivy::Result<Self::Fruit> {
        let mut heap = BinaryHeap::with_capacity(self.limit);
        for child_fruit in child_fruits {
            for document in child_fruit? {
                retain_smallest(&mut heap, document, self.limit);
            }
        }
        let mut documents = heap.into_vec();
        documents.sort_unstable();
        Ok(documents)
    }
}

struct StableReferenceSegmentCollector {
    limit: usize,
    segment_ord: u32,
    stable_ids: StrColumn,
    payload_offsets: Column<u64>,
    payload_lengths: Column<u64>,
    payload_digests: StrColumn,
    after_stable_id: Option<Arc<str>>,
    scratch: String,
    digest_scratch: String,
    heap: BinaryHeap<StableReferenceDocument>,
    error: Option<tantivy::TantivyError>,
}

impl SegmentCollector for StableReferenceSegmentCollector {
    type Fruit = tantivy::Result<Vec<StableReferenceDocument>>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        if self.error.is_some() || self.limit == 0 {
            return;
        }
        self.scratch.clear();
        let mut ordinals = self.stable_ids.term_ords(doc);
        let Some(ordinal) = ordinals.next() else {
            self.error = Some(tantivy::TantivyError::InternalError(
                "reference document is missing its stable ID".into(),
            ));
            return;
        };
        if ordinals.next().is_some() {
            self.error = Some(tantivy::TantivyError::InternalError(
                "reference document has multiple stable IDs".into(),
            ));
            return;
        }
        match self.stable_ids.ord_to_str(ordinal, &mut self.scratch) {
            Ok(true) => {}
            Ok(false) => {
                self.error = Some(tantivy::TantivyError::InternalError(
                    "reference document has an invalid stable ID ordinal".into(),
                ));
                return;
            }
            Err(error) => {
                self.error = Some(error.into());
                return;
            }
        }
        let payload_offset = match required_fast_u64(&self.payload_offsets, doc, "payload_offset") {
            Ok(value) => value,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let payload_length = match required_fast_u64(&self.payload_lengths, doc, "payload_length") {
            Ok(value) => value,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        self.digest_scratch.clear();
        let mut digest_ordinals = self.payload_digests.term_ords(doc);
        let Some(digest_ordinal) = digest_ordinals.next() else {
            self.error = Some(tantivy::TantivyError::InternalError(
                "reference document is missing its payload digest".into(),
            ));
            return;
        };
        if digest_ordinals.next().is_some() {
            self.error = Some(tantivy::TantivyError::InternalError(
                "reference document has multiple payload digests".into(),
            ));
            return;
        }
        match self
            .payload_digests
            .ord_to_str(digest_ordinal, &mut self.digest_scratch)
        {
            Ok(true) => {}
            Ok(false) => {
                self.error = Some(tantivy::TantivyError::InternalError(
                    "reference document has an invalid payload digest ordinal".into(),
                ));
                return;
            }
            Err(error) => {
                self.error = Some(error.into());
                return;
            }
        }
        let payload_digest = match payload_digest_from_hex(&self.digest_scratch) {
            Ok(digest) => digest,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        if self
            .after_stable_id
            .as_deref()
            .is_some_and(|after| self.scratch.as_str() <= after)
        {
            return;
        }
        retain_smallest(
            &mut self.heap,
            StableReferenceDocument {
                stable_id: self.scratch.clone(),
                address: DocAddress::new(self.segment_ord, doc),
                payload_offset,
                payload_length,
                payload_digest,
            },
            self.limit,
        );
    }

    fn harvest(self) -> Self::Fruit {
        if let Some(error) = self.error {
            return Err(error);
        }
        let mut documents = self.heap.into_vec();
        documents.sort_unstable();
        Ok(documents)
    }
}

fn payload_digest_from_hex(encoded: &str) -> tantivy::Result<DigestV1> {
    if encoded.len() != DigestV1::BYTE_LEN * 2 {
        return Err(tantivy::TantivyError::InternalError(
            "reference payload digest has an invalid encoded length".to_owned(),
        ));
    }
    let mut bytes = [0_u8; DigestV1::BYTE_LEN];
    hex::decode_to_slice(encoded, &mut bytes).map_err(|_| {
        tantivy::TantivyError::InternalError(
            "reference payload digest is not valid lowercase hexadecimal".to_owned(),
        )
    })?;
    Ok(DigestV1::from_bytes(bytes))
}

fn required_fast_u64(
    column: &Column<u64>,
    document: DocId,
    field_name: &'static str,
) -> tantivy::Result<u64> {
    exactly_one_fast_u64(column.values_for_doc(document), field_name)
}

fn exactly_one_fast_u64(
    mut values: impl Iterator<Item = u64>,
    field_name: &'static str,
) -> tantivy::Result<u64> {
    let Some(value) = values.next() else {
        return Err(tantivy::TantivyError::InternalError(format!(
            "reference document is missing {field_name} fast field value"
        )));
    };
    if values.next().is_some() {
        return Err(tantivy::TantivyError::InternalError(format!(
            "reference document has multiple {field_name} fast field values"
        )));
    }
    Ok(value)
}

fn retain_smallest(
    heap: &mut BinaryHeap<StableReferenceDocument>,
    candidate: StableReferenceDocument,
    limit: usize,
) {
    if limit == 0 {
        return;
    }
    if heap.len() < limit {
        heap.push(candidate);
        return;
    }
    if heap.peek().is_some_and(|largest| candidate < *largest) {
        let _ = heap.pop();
        heap.push(candidate);
    }
}

#[derive(Debug)]
struct DecodedReferencePayload {
    document: ReferencePayload,
    payload_bytes: usize,
}

fn decode_reference_payload(
    payloads: &ReferencePayloadReader,
    selected: &StableReferenceDocument,
    maximum_payload_bytes: usize,
    remaining_page_payload_bytes: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Option<DecodedReferencePayload>, ReferenceQueryError> {
    let range = payloads
        .validate_range(
            selected.payload_offset,
            selected.payload_length,
            maximum_payload_bytes,
        )
        .map_err(|source| ReferenceQueryError::Payload {
            stable_id: selected.stable_id.clone(),
            source,
        })?;
    let payload_bytes = range.encoded_bytes();
    if payload_bytes > remaining_page_payload_bytes {
        return Ok(None);
    }

    let document = payloads
        .read(range, selected.payload_digest, &selected.stable_id, budget)
        .map_err(|source| ReferenceQueryError::Payload {
            stable_id: selected.stable_id.clone(),
            source,
        })?;
    document.validate(&selected.stable_id).map_err(|source| {
        ReferenceQueryError::CorruptDocument {
            stable_id: Some(selected.stable_id.clone()),
            reason: source.to_string(),
        }
    })?;
    Ok(Some(DecodedReferencePayload {
        document,
        payload_bytes,
    }))
}

#[derive(Default)]
struct JsonLengthWriter {
    bytes: usize,
}

impl Write for JsonLengthWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn json_bytes_for(value: &impl Serialize) -> Result<usize, serde_json::Error> {
    let mut writer = JsonLengthWriter::default();
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}

fn reference_hit_json_bytes(hit: &ReferenceHit) -> Result<usize, ReferenceQueryError> {
    json_bytes_for(hit).map_err(|error| ReferenceQueryError::CorruptDocument {
        stable_id: Some(hit.stable_id.clone()),
        reason: format!("reference hit cannot be serialized for response budgeting: {error}"),
    })
}

fn reference_hit(
    stored: ReferencePayload,
    storage: GenerationStorageContract,
) -> Result<ReferenceHit, ReferenceQueryError> {
    let object_count = 1usize.saturating_add(resolution_object_count(&stored.fact.resolution));
    if object_count > MAX_REFERENCE_OBJECTS_PER_HIT {
        return Err(ReferenceQueryError::ResponseHitObjectLimitExceeded {
            stable_id: stored.stable_id,
            actual: object_count,
            maximum: MAX_REFERENCE_OBJECTS_PER_HIT,
        });
    }

    let context = ReferenceContext {
        doc_file_id: stored.source_file_id,
        doc_class_id: stored.source_class_id,
        object_name: None,
        hierarchy_path: None,
        field_hint: Some(stored.fact.field_path.to_string()),
        source_line: None,
        source_column: None,
    };
    let mut raw_object = raw_reference_object(&stored, storage)?;
    raw_object
        .field_hints
        .push(resolution_hint(&stored.fact.resolution).to_owned());
    let mut objects = vec![raw_object];
    objects.extend(resolution_objects(&stored, storage)?);

    Ok(ReferenceHit {
        source_path: wire::portable_path_string(stored.source_path.clone())?,
        source_kind: stored.source_kind,
        stable_id: stored.stable_id,
        location: wire::location(
            stored.source_path,
            stored.source_guid,
            stored.source_file_id,
            stored.source_class_id,
        )?,
        contexts: vec![context],
        objects,
    })
}

fn resolution_object_count(resolution: &ReferenceResolutionProjection) -> usize {
    match resolution {
        ReferenceResolutionProjection::Null
        | ReferenceResolutionProjection::Missing { target: None }
        | ReferenceResolutionProjection::Unloaded { source: None }
        | ReferenceResolutionProjection::Invalid => 0,
        ReferenceResolutionProjection::Resolved { .. }
        | ReferenceResolutionProjection::Missing { target: Some(_) }
        | ReferenceResolutionProjection::Unloaded { source: Some(_) } => 1,
        ReferenceResolutionProjection::Ambiguous { candidates } => candidates.len(),
    }
}

const fn resolution_hint(resolution: &ReferenceResolutionProjection) -> &'static str {
    match resolution {
        ReferenceResolutionProjection::Null => "resolution.null",
        ReferenceResolutionProjection::Resolved { .. } => "resolution.resolved",
        ReferenceResolutionProjection::Unloaded { .. } => "resolution.unloaded",
        ReferenceResolutionProjection::Missing { .. } => "resolution.missing",
        ReferenceResolutionProjection::Ambiguous { .. } => "resolution.ambiguous",
        ReferenceResolutionProjection::Invalid => "resolution.invalid",
    }
}

fn raw_reference_object(
    stored: &ReferencePayload,
    storage: GenerationStorageContract,
) -> Result<ReferenceObject, ReferenceQueryError> {
    match &stored.fact.raw_target {
        RawReferenceProjection::Binary {
            file_id,
            path_id,
            external,
        } => {
            let target_guid = external
                .as_ref()
                .and_then(|external| external.guid)
                .map(hex::encode);
            let target_address = if external.is_none() && *path_id != 0 {
                stored.source_object.as_ref().and_then(|source| {
                    ObjectAddress::binary_at(source.source_locator().clone(), *path_id).ok()
                })
            } else {
                None
            };
            let stable_id = if let Some(address) = &target_address {
                reference_object_key_for(storage, address)
            } else if let Some(guid) = target_guid.as_deref() {
                reference_guid_key(guid, Some(*path_id))
            } else {
                raw_target_stable_id(&stored.fact.raw_target)?
            };
            let path = external
                .as_ref()
                .map(|external| external.path.as_str())
                .filter(|path| !path.is_empty())
                .unwrap_or(&stored.source_path)
                .to_owned();
            let mut field_hints = vec![
                format!("raw.binary.file_id={file_id}"),
                format!("raw.binary.path_id={path_id}"),
            ];
            if let Some(external) = external {
                field_hints.push(format!("raw.binary.external_index={}", external.index));
                field_hints.push(format!("raw.binary.external_type_id={}", external.type_id));
            }
            Ok(ReferenceObject {
                doc_file_id: Some(*path_id),
                doc_class_id: None,
                stable_id,
                location: wire::location(
                    path,
                    target_guid.or_else(|| target_address.as_ref().and(stored.source_guid.clone())),
                    Some(*path_id),
                    None,
                )?,
                object_name: None,
                hierarchy_path: None,
                field_hints,
            })
        }
        RawReferenceProjection::Yaml {
            file_id,
            guid,
            type_id,
        } => {
            let target_guid = guid.as_ref().map(|guid| match guid {
                GuidProjection::Parsed(bytes) => hex::encode(bytes),
                GuidProjection::Invalid(value) => value.clone(),
            });
            let target_address = match (target_guid.as_ref(), file_id, &stored.source_object) {
                (None, Some(file_id), Some(source)) => {
                    YamlFileId::new(*file_id).ok().and_then(|file_id| {
                        ObjectAddress::yaml(source.source_locator().clone(), file_id).ok()
                    })
                }
                _ => None,
            };
            let stable_id = if let Some(address) = &target_address {
                reference_object_key_for(storage, address)
            } else if let Some(guid) = target_guid.as_deref() {
                reference_guid_key(guid, *file_id)
            } else {
                raw_target_stable_id(&stored.fact.raw_target)?
            };
            let mut field_hints = Vec::new();
            if let Some(file_id) = file_id {
                field_hints.push(format!("raw.yaml.file_id={file_id}"));
            }
            if let Some(type_id) = type_id {
                field_hints.push(format!("raw.yaml.type_id={type_id}"));
            }
            Ok(ReferenceObject {
                doc_file_id: *file_id,
                doc_class_id: None,
                stable_id,
                location: wire::location(
                    stored.source_path.clone(),
                    target_guid.or_else(|| target_address.as_ref().and(stored.source_guid.clone())),
                    *file_id,
                    None,
                )?,
                object_name: None,
                hierarchy_path: None,
                field_hints,
            })
        }
    }
}

fn resolution_objects(
    stored: &ReferencePayload,
    storage: GenerationStorageContract,
) -> Result<Vec<ReferenceObject>, ReferenceQueryError> {
    let mut objects = Vec::new();
    match &stored.fact.resolution {
        ReferenceResolutionProjection::Null
        | ReferenceResolutionProjection::Missing { target: None }
        | ReferenceResolutionProjection::Invalid => {}
        ReferenceResolutionProjection::Resolved { target } => {
            objects.push(address_object(target, "resolution.resolved", storage)?);
        }
        ReferenceResolutionProjection::Missing {
            target: Some(target),
        } => {
            objects.push(address_object(target, "resolution.missing", storage)?);
        }
        ReferenceResolutionProjection::Ambiguous { candidates } => {
            objects.extend(
                candidates
                    .iter()
                    .map(|target| address_object(target, "resolution.ambiguous", storage))
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        ReferenceResolutionProjection::Unloaded {
            source: Some(source),
        } => {
            let stable_id = source_stable_id(source)?;
            objects.push(ReferenceObject {
                doc_file_id: None,
                doc_class_id: None,
                stable_id,
                location: wire::locator_location(source, None, None, None)?,
                object_name: None,
                hierarchy_path: None,
                field_hints: vec!["resolution.unloaded".to_owned()],
            });
        }
        ReferenceResolutionProjection::Unloaded { source: None } => {}
    }
    Ok(objects)
}

fn address_object(
    address: &ObjectAddress,
    resolution: &str,
    storage: GenerationStorageContract,
) -> Result<ReferenceObject, ReferenceQueryError> {
    let file_id = address
        .binary_path_id()
        .or_else(|| address.yaml_file_id().map(YamlFileId::get));
    Ok(ReferenceObject {
        doc_file_id: file_id,
        doc_class_id: None,
        stable_id: reference_object_key_for(storage, address),
        location: wire::locator_location(address.source_locator(), None, file_id, None)?,
        object_name: None,
        hierarchy_path: None,
        field_hints: vec![resolution.to_owned()],
    })
}

fn raw_target_stable_id(target: &RawReferenceProjection) -> Result<String, ReferenceQueryError> {
    let encoded =
        serde_json::to_vec(target).map_err(|error| ReferenceQueryError::CorruptDocument {
            stable_id: None,
            reason: format!("raw reference target cannot be serialized: {error}"),
        })?;
    Ok(format!(
        "raw-reference-v1:{}",
        hex::encode(DigestV1::hash_bytes(&encoded).as_bytes())
    ))
}

fn source_stable_id(source: &SourceLocator) -> Result<String, ReferenceQueryError> {
    let encoded =
        serde_json::to_vec(source).map_err(|error| ReferenceQueryError::CorruptDocument {
            stable_id: None,
            reason: format!("unloaded source locator cannot be serialized: {error}"),
        })?;
    Ok(format!(
        "unloaded-source-v1:{}",
        hex::encode(DigestV1::hash_bytes(&encoded).as_bytes())
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tantivy::{Index, TantivyDocument};
    use tempfile::tempdir;
    use unity_asset_core::{
        AssetLoadLimits, AssetLoadUsage, DiagnosticSeverity, FieldPath, WorkspaceId,
        WorkspaceRevision,
    };
    use unity_asset_search_protocol::ValidateContract;

    use super::*;
    use crate::analysis::{
        BinaryExternalProjection, ReferenceDependencyKey, ReferenceProjectionFact,
    };
    use crate::generation::SearchGenerationId;
    use crate::projection::{GenerationProjection, ProjectionMetrics, ReferenceDocument};
    use crate::reference_payload::REFERENCE_PAYLOAD_FILE;
    use crate::store::{ProjectionReaders, ProjectionStore};

    const GUID: &str = "abababababababababababababababab";

    fn generation(label: &[u8]) -> GenerationStamp {
        GenerationStamp::current(
            SearchGenerationId::new(DigestV1::hash_bytes(label)),
            WorkspaceId::from_u128(1).unwrap(),
            WorkspaceRevision::new(DigestV1::hash_bytes(b"revision")),
        )
    }

    fn source_address(path_id: i64) -> ObjectAddress {
        ObjectAddress::binary_direct(
            SourceLocator::path(format!("Assets/Source{path_id}.asset")).unwrap(),
            path_id,
        )
        .unwrap()
    }

    fn projected_reference(stable_id: &str, source_path_id: i64) -> ReferenceDocument {
        let source_object = source_address(source_path_id);
        let target_guid = [0xab; 16];
        ReferenceDocument {
            stable_id: stable_id.to_owned(),
            source_path: format!("Assets/Source{source_path_id}.asset"),
            source_kind: "SerializedAsset".to_owned(),
            source_guid: Some(format!("source-guid-{source_path_id}")),
            source_object: Some(source_object.clone()),
            source_file_id: Some(source_path_id),
            source_class_id: Some(-3),
            fact: ReferenceProjectionFact {
                source_object: Some(source_object.clone()),
                source_file_id: Some(source_path_id),
                source_class_id: Some(-3),
                field_path: FieldPath::root().push_field("m_Target").unwrap(),
                raw_target: RawReferenceProjection::Binary {
                    file_id: -4,
                    path_id: -99,
                    external: Some(BinaryExternalProjection {
                        index: 0,
                        guid: Some(target_guid),
                        type_id: -8,
                        path: "Packages/External.asset".to_owned(),
                    }),
                },
                resolution: ReferenceResolutionProjection::Missing {
                    target: Some(
                        ObjectAddress::binary_direct(
                            SourceLocator::path("Packages/External.asset").unwrap(),
                            -99,
                        )
                        .unwrap(),
                    ),
                },
                diagnostics: Vec::new(),
                dependency_keys: vec![ReferenceDependencyKey::Guid {
                    guid: GUID.to_owned(),
                    file_id: Some(-99),
                }],
            },
            incoming_keys: vec![
                reference_guid_key(GUID, None),
                reference_guid_key(GUID, Some(-99)),
            ],
            outgoing_keys: vec![reference_object_key(&source_object)],
        }
    }

    fn stored_reference(projected: ReferenceDocument) -> ReferencePayload {
        ReferencePayload::from_document(projected)
    }

    fn projection(documents: Vec<ReferenceDocument>) -> GenerationProjection {
        GenerationProjection {
            search_documents: Vec::new(),
            reference_documents: documents,
            diagnostics: Vec::new(),
            truncations: Vec::new(),
            metrics: ProjectionMetrics::default(),
        }
    }

    fn engine(
        documents: Vec<ReferenceDocument>,
        stamp: GenerationStamp,
    ) -> (tempfile::TempDir, ReferenceQueryEngine) {
        engine_with_completeness(documents, stamp, ReferenceQueryCompleteness::complete())
    }

    fn engine_with_completeness(
        documents: Vec<ReferenceDocument>,
        stamp: GenerationStamp,
        completeness: ReferenceQueryCompleteness,
    ) -> (tempfile::TempDir, ReferenceQueryEngine) {
        let directory = tempdir().unwrap();
        ProjectionStore::build(directory.path(), &projection(documents)).unwrap();
        let readers =
            ProjectionReaders::open(directory.path(), &mut AssetLoadBudget::default()).unwrap();
        let snapshot = ReferenceQuerySnapshot::new(stamp, readers.references(), completeness);
        (directory, ReferenceQueryEngine::new(Arc::new(snapshot)))
    }

    fn open_engine(directory: &Path, stamp: GenerationStamp) -> ReferenceQueryEngine {
        let readers = ProjectionReaders::open(directory, &mut AssetLoadBudget::default()).unwrap();
        let snapshot = ReferenceQuerySnapshot::new(
            stamp,
            readers.references(),
            ReferenceQueryCompleteness::complete(),
        );
        ReferenceQueryEngine::new(Arc::new(snapshot))
    }

    fn payload_path(directory: &Path) -> PathBuf {
        directory.join("references").join(REFERENCE_PAYLOAD_FILE)
    }

    #[test]
    fn payload_fast_fields_require_exactly_one_value() {
        let missing = exactly_one_fast_u64(std::iter::empty(), "payload_offset").unwrap_err();
        let duplicate =
            exactly_one_fast_u64([7_u64, 8_u64].into_iter(), "payload_length").unwrap_err();

        assert!(missing.to_string().contains("missing payload_offset"));
        assert!(duplicate.to_string().contains("multiple payload_length"));
        assert_eq!(
            exactly_one_fast_u64([9_u64].into_iter(), "payload_offset").unwrap(),
            9
        );
    }

    #[test]
    fn completeness_deduplicates_borrowed_diagnostics_and_accounts_retained_storage() {
        let address = ObjectAddress::yaml(
            SourceLocator::archive_member("Assets/Archive.zip", "Nested.asset").unwrap(),
            "12345".parse().unwrap(),
        )
        .unwrap();
        let field_path = FieldPath::root()
            .push_field("m_Target")
            .unwrap()
            .push_index(2)
            .unwrap();
        let addressed = Diagnostic::new(
            DiagnosticSeverity::Warning,
            "SEARCH_REFERENCE_MISSING",
            "the referenced object is unavailable",
        )
        .unwrap()
        .at_address(address)
        .at_field(field_path);
        let general = Diagnostic::new(
            DiagnosticSeverity::Info,
            "SEARCH_REFERENCE_PARTIAL",
            "reference coverage is partial",
        )
        .unwrap();
        let input = [addressed.clone(), general.clone(), addressed.clone()];
        let mut budget = AssetLoadBudget::default();

        let completeness =
            ReferenceQueryCompleteness::new(false, true, input.iter(), &mut budget).unwrap();

        let mut expected = vec![addressed, general];
        expected.sort_unstable();
        assert_eq!(completeness.diagnostics(), expected.as_slice());
        assert!(!completeness.is_complete());

        let retained_addressed = completeness
            .diagnostics()
            .iter()
            .find(|diagnostic| diagnostic.code() == "SEARCH_REFERENCE_MISSING")
            .unwrap();
        assert_ne!(retained_addressed.code().as_ptr(), input[0].code().as_ptr());
        assert_ne!(
            retained_addressed.message().as_ptr(),
            input[0].message().as_ptr()
        );
        let retained_address = retained_addressed.address().unwrap();
        let input_address = input[0].address().unwrap();
        assert_ne!(
            retained_address
                .source_locator()
                .root_alias()
                .as_str()
                .as_ptr(),
            input_address
                .source_locator()
                .root_alias()
                .as_str()
                .as_ptr()
        );
        let FieldPathSegment::Field(retained_field) =
            &retained_addressed.field_path().unwrap().segments()[0]
        else {
            panic!("the retained field path must start with a field");
        };
        let FieldPathSegment::Field(input_field) = &input[0].field_path().unwrap().segments()[0]
        else {
            panic!("the input field path must start with a field");
        };
        assert_ne!(retained_field.as_ptr(), input_field.as_ptr());

        let diagnostic_backing = expected
            .iter()
            .map(|diagnostic| {
                checked_string_bytes(diagnostic.code().len()).unwrap()
                    + checked_string_bytes(diagnostic.message().len()).unwrap()
                    + diagnostic
                        .address()
                        .map(|address| {
                            usize_to_budget_bytes(address.retained_clone_bytes().unwrap()).unwrap()
                        })
                        .unwrap_or_default()
                    + diagnostic
                        .field_path()
                        .map(|path| {
                            usize_to_budget_bytes(path.retained_clone_bytes().unwrap()).unwrap()
                        })
                        .unwrap_or_default()
            })
            .sum::<u64>();
        let arc_bytes = checked_arc_slice_bytes::<Diagnostic>(expected.len()).unwrap();
        let expected_bytes = checked_vec_bytes::<&Diagnostic>(input.len())
            .unwrap()
            .checked_add(checked_vec_bytes::<Diagnostic>(expected.len()).unwrap())
            .and_then(|bytes| bytes.checked_add(arc_bytes))
            .and_then(|bytes| bytes.checked_add(diagnostic_backing))
            .unwrap();
        assert_eq!(
            budget.usage(),
            AssetLoadUsage {
                entries: 3,
                bytes: expected_bytes,
                members: 2,
                ..AssetLoadUsage::default()
            }
        );
    }

    #[test]
    fn completeness_budget_failure_is_typed_and_precedes_deep_clone_allocation() {
        let diagnostic = Diagnostic::new(
            DiagnosticSeverity::Warning,
            "SEARCH_REFERENCE_MISSING",
            "the referenced object is unavailable",
        )
        .unwrap();
        let temporary_bytes = checked_vec_bytes::<&Diagnostic>(1).unwrap();
        let retained_bytes = retained_diagnostic_bytes(&[&diagnostic]).unwrap();
        let requested_bytes = temporary_bytes.checked_add(retained_bytes).unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_bytes: requested_bytes - 1,
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error =
            ReferenceQueryCompleteness::new(true, true, [&diagnostic], &mut budget).unwrap_err();

        assert!(matches!(
            &error,
            ReferenceQueryCompletenessError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if *limit == requested_bytes - 1 && *requested == requested_bytes
        ));
        assert!(matches!(
            error
                .source()
                .and_then(|source| source.downcast_ref::<BudgetError>()),
            Some(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert_eq!(
            budget.usage(),
            AssetLoadUsage {
                entries: 1,
                bytes: temporary_bytes,
                ..AssetLoadUsage::default()
            }
        );
    }

    #[test]
    fn complete_uses_the_zero_diagnostic_path() {
        let completeness = ReferenceQueryCompleteness::complete();

        assert!(completeness.is_complete());
        assert!(completeness.diagnostics.is_none());
        assert!(completeness.diagnostics().is_empty());
        assert!(!completeness.diagnostics_truncated());
        assert_eq!(completeness.diagnostic_json_bytes(), 2);
    }

    #[test]
    fn completeness_diagnostic_projection_has_exact_count_boundary() {
        let diagnostics = (0..=MAX_REFERENCE_RESPONSE_DIAGNOSTICS)
            .map(|index| {
                Diagnostic::new(
                    DiagnosticSeverity::Warning,
                    "SEARCH_REFERENCE_PARTIAL",
                    format!("diagnostic {index}"),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();

        let exact = ReferenceQueryCompleteness::new(
            false,
            true,
            diagnostics[..MAX_REFERENCE_RESPONSE_DIAGNOSTICS].iter(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(
            exact.diagnostics().len(),
            MAX_REFERENCE_RESPONSE_DIAGNOSTICS
        );
        assert!(!exact.diagnostics_truncated());
        assert_eq!(
            serde_json::to_vec(exact.diagnostics()).unwrap().len(),
            exact.diagnostic_json_bytes()
        );

        let one_over = ReferenceQueryCompleteness::new(
            false,
            true,
            diagnostics.iter(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(
            one_over.diagnostics().len(),
            MAX_REFERENCE_RESPONSE_DIAGNOSTICS
        );
        assert!(one_over.diagnostics_truncated());
        assert!(one_over.diagnostic_json_bytes() <= MAX_REFERENCE_RESPONSE_DIAGNOSTIC_JSON_BYTES);
    }

    #[test]
    fn response_diagnostics_have_exact_byte_and_count_boundaries() {
        let first = Diagnostic::new(
            DiagnosticSeverity::Warning,
            "SEARCH_REFERENCE_PARTIAL",
            "first response diagnostic",
        )
        .unwrap();
        let second = Diagnostic::new(
            DiagnosticSeverity::Warning,
            "SEARCH_REFERENCE_MISSING",
            "second response diagnostic",
        )
        .unwrap();
        let exact_bytes = serde_json::to_vec(&vec![first.clone(), second.clone()])
            .unwrap()
            .len();
        let mut document = projected_reference("reference-a", -7);
        document.fact.diagnostics = vec![first, second];
        let (_directory, engine) = engine(vec![document], generation(b"response-diagnostics"));

        let exact = engine
            .references_with_limits(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1),
                ReferenceQueryLimits {
                    max_response_diagnostics: 2,
                    max_response_diagnostic_json_bytes: exact_bytes,
                    ..REFERENCE_QUERY_LIMITS
                },
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(exact.diagnostics.len(), 2);
        assert_eq!(
            exact.diagnostic_coverage,
            ReferenceDiagnosticCoverage {
                returned: 2,
                truncated: false,
                total: Some(2),
                serialized_bytes: u64::try_from(exact_bytes).unwrap(),
                max_count: 2,
                max_serialized_bytes: u64::try_from(exact_bytes).unwrap(),
            }
        );
        assert_eq!(
            u64::try_from(serde_json::to_vec(&exact.diagnostics).unwrap().len()).unwrap(),
            exact.diagnostic_coverage.serialized_bytes
        );
        assert!(
            serde_json::to_value(&exact)
                .unwrap()
                .get("diagnostic_coverage")
                .is_some()
        );

        let one_over = engine
            .references_with_limits(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1),
                ReferenceQueryLimits {
                    max_response_diagnostics: 2,
                    max_response_diagnostic_json_bytes: exact_bytes - 1,
                    ..REFERENCE_QUERY_LIMITS
                },
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(one_over.diagnostics.len(), 1);
        assert!(one_over.diagnostic_coverage.truncated);
        assert_eq!(one_over.diagnostic_coverage.total, Some(2));
        assert!(
            one_over.diagnostic_coverage.serialized_bytes
                <= one_over.diagnostic_coverage.max_serialized_bytes
        );
    }

    #[test]
    fn negative_ids_round_trip_without_source_io() {
        let stamp = generation(b"one");
        let (_directory, engine) = engine(vec![projected_reference("reference-a", -7)], stamp);
        let response = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 10),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(
            response.request.selector,
            ReferenceSelector::Guid {
                guid: GUID.to_owned(),
                file_id: Some(-99),
            }
        );
        assert_eq!(response.hits.len(), 1);
        assert_eq!(response.hits[0].location.file_id, Some(-7));
        assert_eq!(response.hits[0].location.class_id, Some(-3));
        assert_eq!(response.hits[0].contexts[0].doc_file_id, Some(-7));
        assert_eq!(
            response.hits[0].contexts[0].field_hint.as_deref(),
            Some("$.m_Target")
        );
        let raw = &response.hits[0].objects[0];
        assert_eq!(raw.location.file_id, Some(-99));
        assert_eq!(raw.location.guid.as_deref(), Some(GUID));
        assert!(
            raw.field_hints
                .iter()
                .any(|hint| hint == "raw.binary.file_id=-4")
        );
        assert!(
            raw.field_hints
                .iter()
                .any(|hint| hint == "raw.binary.path_id=-99")
        );
    }

    #[test]
    fn uppercase_guid_is_rejected_instead_of_silently_normalized() {
        let stamp = generation(b"uppercase-guid");
        let (_directory, engine) = engine(Vec::new(), stamp);
        let error = engine
            .references(
                ReferenceRequest::incoming_guid(GUID.to_ascii_uppercase(), None, 10),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert!(matches!(error, ReferenceQueryError::InvalidGuid));
        assert_eq!(error.api_code(), ApiErrorCode::InvalidRequest);
    }

    #[test]
    fn malformed_guid_is_rejected_instead_of_reported_as_no_matches() {
        let stamp = generation(b"invalid-guid");
        let (_directory, engine) = engine(Vec::new(), stamp);

        let error = engine
            .references(
                ReferenceRequest::incoming_guid("not-a-guid", None, 10),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert!(matches!(error, ReferenceQueryError::InvalidGuid));
        assert_eq!(error.api_code(), ApiErrorCode::InvalidRequest);
    }

    #[test]
    fn oversized_guid_is_rejected_without_consuming_query_budget() {
        let stamp = generation(b"oversized-guid");
        let (_directory, engine) = engine(Vec::new(), stamp);
        let mut budget = AssetLoadBudget::default();

        let error = engine
            .references(
                ReferenceRequest::incoming_guid("a".repeat(64 * 1024), None, 10),
                &mut budget,
            )
            .unwrap_err();

        assert!(matches!(error, ReferenceQueryError::InvalidGuid));
        assert_eq!(error.api_code(), ApiErrorCode::InvalidRequest);
        assert_eq!(budget.usage(), AssetLoadUsage::default());
    }

    #[test]
    fn oversized_cursor_is_rejected_before_query_execution() {
        let stamp = generation(b"oversized-cursor");
        let generation = stamp.generation;
        let (_directory, engine) = engine(Vec::new(), stamp);
        let binding = ReferenceRequest::incoming_guid(GUID, None, 10)
            .cursor_query_binding()
            .unwrap();
        let mut budget = AssetLoadBudget::default();

        let error = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, None, 10).with_cursor(ReferenceCursor {
                    generation: wire::generation_id(generation),
                    query_policy_id: wire::query_policy_id(),
                    after_stable_id: "x".repeat(MAX_REFERENCE_CURSOR_STABLE_ID_BYTES + 1),
                    query_binding: binding,
                }),
                &mut budget,
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            ReferenceQueryError::CursorStableIdTooLong {
                actual,
                maximum: MAX_REFERENCE_CURSOR_STABLE_ID_BYTES,
            } if *actual == MAX_REFERENCE_CURSOR_STABLE_ID_BYTES + 1
        ));
        assert_eq!(error.api_code(), ApiErrorCode::InvalidCursor);
        assert_eq!(budget.usage(), AssetLoadUsage::default());
    }

    #[test]
    fn incoming_and_outgoing_use_their_respective_key_fields() {
        let document = projected_reference("reference-a", -7);
        let source = document.source_object.clone().unwrap();
        let stamp = generation(b"two");
        let (_directory, engine) = engine(vec![document], stamp);

        let incoming = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, None, 10),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let outgoing = engine
            .references(
                ReferenceRequest {
                    direction: ReferenceDirection::Outgoing,
                    selector: ReferenceSelector::Object { address: source },
                    limit: 10,
                    cursor: None,
                },
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(incoming.hits.len(), 1);
        assert_eq!(outgoing.hits.len(), 1);
        assert_eq!(incoming.hits[0].stable_id, outgoing.hits[0].stable_id);
    }

    #[test]
    fn stable_pagination_uses_limit_plus_one_and_generation_cursor() {
        let stamp = generation(b"three");
        let documents = vec![
            projected_reference("reference-a", -7),
            projected_reference("reference-b", -8),
            projected_reference("reference-c", -9),
        ];
        let (_directory, engine) = engine(documents, stamp.clone());

        let first = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        first.validate().unwrap();
        assert_eq!(first.hits[0].stable_id, "reference-a");
        assert_eq!(first.coverage.total, Some(3));
        assert!(first.coverage.truncated);
        let cursor = first.coverage.next_cursor.unwrap();
        assert_eq!(cursor.generation, wire::generation_id(stamp.generation));
        assert_eq!(cursor.query_policy_id, wire::query_policy_id());
        assert!(is_valid_cursor_query_binding(&cursor.query_binding));
        let cursor: ReferenceCursor =
            serde_json::from_value(serde_json::to_value(cursor).unwrap()).unwrap();

        let second = engine
            .references(
                ReferenceRequest {
                    direction: ReferenceDirection::Incoming,
                    selector: ReferenceSelector::Guid {
                        guid: GUID.to_owned(),
                        file_id: Some(-99),
                    },
                    limit: 1,
                    cursor: Some(cursor),
                },
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(second.hits[0].stable_id, "reference-b");
    }

    #[test]
    fn cursor_from_another_generation_is_a_typed_error() {
        let stamp = generation(b"active");
        let (_directory, engine) = engine(vec![projected_reference("reference-a", -7)], stamp);
        let error = engine
            .references(
                ReferenceRequest {
                    direction: ReferenceDirection::Incoming,
                    selector: ReferenceSelector::Guid {
                        guid: GUID.to_owned(),
                        file_id: None,
                    },
                    limit: 10,
                    cursor: Some(ReferenceCursor {
                        generation: wire::generation_id(generation(b"old").generation),
                        query_policy_id: wire::query_policy_id(),
                        after_stable_id: "reference-a".to_owned(),
                        query_binding: ReferenceRequest::incoming_guid(GUID, None, 10)
                            .cursor_query_binding()
                            .unwrap(),
                    }),
                },
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            ReferenceQueryError::CursorGenerationMismatch { .. }
        ));
        assert_eq!(error.api_code(), ApiErrorCode::StaleCursor);
    }

    #[test]
    fn cursor_from_another_query_policy_is_a_typed_error() {
        let stamp = generation(b"active-policy");
        let active_generation = wire::generation_id(stamp.generation);
        let (_directory, engine) = engine(vec![projected_reference("reference-a", -7)], stamp);
        let cursor = ReferenceCursor {
            generation: active_generation,
            query_policy_id: QueryPolicyId::from_bytes([0xa5; 32]),
            after_stable_id: "reference-a".to_owned(),
            query_binding: ReferenceRequest::incoming_guid(GUID, None, 10)
                .cursor_query_binding()
                .unwrap(),
        };

        let error = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, None, 10).with_cursor(cursor),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            ReferenceQueryError::CursorQueryPolicyMismatch { .. }
        ));
        assert_eq!(error.api_code(), ApiErrorCode::StaleCursor);
    }

    #[test]
    fn cursor_without_required_bindings_is_rejected_by_the_protocol() {
        let stamp = generation(b"legacy-cursor");
        let result = serde_json::from_value::<ReferenceCursor>(serde_json::json!({
            "generation": wire::generation_id(stamp.generation),
            "after_stable_id": "reference-a",
        }));

        assert!(result.is_err());
    }

    #[test]
    fn cursor_cannot_cross_reference_directions() {
        let stamp = generation(b"cursor-direction");
        let documents = vec![
            projected_reference("reference-a", -7),
            projected_reference("reference-b", -8),
        ];
        let (_directory, engine) = engine(documents, stamp);
        let cursor = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1),
                &mut AssetLoadBudget::default(),
            )
            .unwrap()
            .coverage
            .next_cursor
            .unwrap();

        let error = engine
            .references(
                ReferenceRequest::outgoing_guid(GUID, Some(-99), 1).with_cursor(cursor),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert!(matches!(&error, ReferenceQueryError::CursorQueryMismatch));
        assert_eq!(error.api_code(), ApiErrorCode::InvalidCursor);
    }

    #[test]
    fn cursor_cannot_cross_normalized_selectors() {
        let stamp = generation(b"cursor-selector");
        let documents = vec![
            projected_reference("reference-a", -7),
            projected_reference("reference-b", -8),
        ];
        let (_directory, engine) = engine(documents, stamp);
        let cursor = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1),
                &mut AssetLoadBudget::default(),
            )
            .unwrap()
            .coverage
            .next_cursor
            .unwrap();

        let error = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, None, 1).with_cursor(cursor),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert!(matches!(&error, ReferenceQueryError::CursorQueryMismatch));
        assert_eq!(error.api_code(), ApiErrorCode::InvalidCursor);
    }

    #[test]
    fn malformed_cursor_query_binding_is_a_typed_error() {
        let stamp = generation(b"malformed-cursor-binding");
        let documents = vec![
            projected_reference("reference-a", -7),
            projected_reference("reference-b", -8),
        ];
        let (_directory, engine) = engine(documents, stamp);
        let mut cursor = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1),
                &mut AssetLoadBudget::default(),
            )
            .unwrap()
            .coverage
            .next_cursor
            .unwrap();
        cursor.query_binding = "reference-query-v2:not-a-digest".to_owned();

        let error = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1).with_cursor(cursor),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            ReferenceQueryError::InvalidCursorQueryBinding
        ));
        assert_eq!(error.api_code(), ApiErrorCode::InvalidCursor);
    }

    #[test]
    fn incomplete_projection_does_not_claim_an_exact_total() {
        let stamp = generation(b"incomplete");
        let mut budget = AssetLoadBudget::default();
        let completeness = ReferenceQueryCompleteness::new(
            false,
            true,
            std::iter::empty::<&Diagnostic>(),
            &mut budget,
        )
        .unwrap();
        let (_directory, engine) = engine_with_completeness(
            vec![projected_reference("reference-a", -7)],
            stamp,
            completeness,
        );

        let response = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, None, 10),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert!(!response.coverage.complete);
        assert!(response.coverage.truncated);
        assert_eq!(response.coverage.total, None);
    }

    #[test]
    fn corrupt_payload_json_is_not_silently_ignored() {
        let directory = tempdir().unwrap();
        ProjectionStore::build(
            directory.path(),
            &projection(vec![projected_reference("reference-a", -7)]),
        )
        .unwrap();
        let path = payload_path(directory.path());
        let mut encoded = fs::read(&path).unwrap();
        encoded[0] = b'!';
        fs::write(&path, encoded).unwrap();
        let engine = open_engine(directory.path(), generation(b"corrupt-payload"));

        let error = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            &error,
            ReferenceQueryError::Payload {
                source: ReferencePayloadReadError::Json(BudgetedJsonError::Json(_)),
                ..
            }
        ));
    }

    #[test]
    fn payload_stable_id_must_match_the_fast_field() {
        let directory = tempdir().unwrap();
        let projected = projected_reference("reference-a", -7);
        ProjectionStore::build(directory.path(), &projection(vec![projected.clone()])).unwrap();
        let path = payload_path(directory.path());
        let mut encoded = fs::read(&path).unwrap();
        let stable_id = b"reference-a";
        let offset = encoded
            .windows(stable_id.len())
            .position(|window| window == stable_id)
            .unwrap();
        encoded[offset..offset + stable_id.len()].copy_from_slice(b"reference-x");
        fs::write(&path, &encoded).unwrap();

        let references = directory.path().join("references");
        let index = Index::open_in_dir(references).unwrap();
        let schema = index.schema();
        let stable_id_field = schema.get_field("stable_id").unwrap();
        let incoming_key_field = schema.get_field("incoming_key").unwrap();
        let outgoing_key_field = schema.get_field("outgoing_key").unwrap();
        let payload_offset_field = schema.get_field("payload_offset").unwrap();
        let payload_length_field = schema.get_field("payload_length").unwrap();
        let payload_digest_field = schema.get_field("payload_digest").unwrap();
        let payload_length = u64::try_from(encoded.len() - 1).unwrap();
        let digest = DigestV1::hash_bytes(&encoded[..encoded.len() - 1]);
        let mut replacement = TantivyDocument::default();
        replacement.add_text(stable_id_field, &projected.stable_id);
        for key in &projected.incoming_keys {
            replacement.add_text(incoming_key_field, key);
        }
        for key in &projected.outgoing_keys {
            replacement.add_text(outgoing_key_field, key);
        }
        replacement.add_u64(payload_offset_field, 0);
        replacement.add_u64(payload_length_field, payload_length);
        replacement.add_text(payload_digest_field, hex::encode(digest.as_bytes()));
        let mut writer = index
            .writer_with_num_threads::<TantivyDocument>(1, 15_000_000)
            .unwrap();
        writer.delete_term(Term::from_field_text(stable_id_field, &projected.stable_id));
        writer.add_document(replacement).unwrap();
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();

        let engine = open_engine(directory.path(), generation(b"stable-id-mismatch"));

        let error = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ReferenceQueryError::CorruptDocument {
                reason,
                ..
            } if reason.contains("stable ID differs")
        ));
    }

    #[test]
    fn deeply_nested_payload_fails_contract_validation() {
        let directory = tempdir().unwrap();
        ProjectionStore::build(
            directory.path(),
            &projection(vec![projected_reference("reference-a", -7)]),
        )
        .unwrap();
        let path = payload_path(directory.path());
        let mut encoded = fs::read(&path).unwrap();
        let payload_length = encoded.iter().position(|byte| *byte == b'\n').unwrap();
        let nesting = 64;
        let mut replacement =
            format!("{}null{}", "[".repeat(nesting), "]".repeat(nesting)).into_bytes();
        assert!(replacement.len() <= payload_length);
        replacement.resize(payload_length, b' ');
        encoded[..payload_length].copy_from_slice(&replacement);
        fs::write(&path, encoded).unwrap();
        let engine = open_engine(directory.path(), generation(b"deep-payload"));

        let error = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ReferenceQueryError::Payload {
                source: ReferencePayloadReadError::Json(
                    BudgetedJsonError::StructureLimitExceeded {
                        resource: "depth",
                        ..
                    }
                ),
                ..
            }
        ));
    }

    #[test]
    fn payload_limit_is_checked_before_materialization() {
        let stamp = generation(b"payload-limit");
        let (_directory, engine) = engine(vec![projected_reference("reference-a", -7)], stamp);
        let limits = ReferenceQueryLimits {
            max_payload_bytes: 1,
            ..REFERENCE_QUERY_LIMITS
        };

        let error = engine
            .references_with_limits(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1),
                limits,
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert_eq!(error.api_code(), ApiErrorCode::Internal);
        assert!(matches!(
            error,
            ReferenceQueryError::Payload {
                source: ReferencePayloadReadError::PayloadTooLarge { maximum: 1, .. },
                ..
            }
        ));
    }

    #[test]
    fn payload_page_budget_truncates_with_a_bound_cursor() {
        let first_document = projected_reference("reference-a", -7);
        let stamp = generation(b"payload-page-budget");
        let documents = vec![first_document, projected_reference("reference-b", -8)];
        let (directory, engine) = engine(documents, stamp);
        let encoded = fs::read(payload_path(directory.path())).unwrap();
        let first_payload_bytes = encoded.iter().position(|byte| *byte == b'\n').unwrap();
        let limits = ReferenceQueryLimits {
            max_page_payload_bytes: first_payload_bytes,
            ..REFERENCE_QUERY_LIMITS
        };

        let first = engine
            .references_with_limits(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 2),
                limits,
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(first.hits.len(), 1);
        assert_eq!(first.hits[0].stable_id, "reference-a");
        assert!(first.coverage.truncated);
        assert!(first.coverage.next_cursor.is_some());
    }

    #[test]
    fn reference_page_accumulates_all_payloads_in_one_caller_budget() {
        let stamp = generation(b"payload-cumulative-budget");
        let documents = vec![
            projected_reference("reference-a", -7),
            projected_reference("reference-b", -8),
        ];
        let (_directory, engine) = engine(documents, stamp);
        let mut first_hit_budget = AssetLoadBudget::default();
        engine
            .references(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1),
                &mut first_hit_budget,
            )
            .expect("measure one decoded hit");
        let first_hit_usage = first_hit_budget.usage();

        let mut page_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: first_hit_usage.bytes,
            ..AssetLoadLimits::default()
        })
        .expect("one-hit page budget");
        let error = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 2),
                &mut page_budget,
            )
            .expect_err("second hit must consume the same page budget");

        assert!(
            matches!(
                &error,
                ReferenceQueryError::Payload {
                    source: ReferencePayloadReadError::Json(BudgetedJsonError::Budget(
                        BudgetError::Exceeded {
                            resource: "bytes",
                            ..
                        }
                    )),
                    ..
                }
            ),
            "unexpected cumulative payload budget error: {error:?}"
        );
        assert!(matches!(
            error
                .source()
                .and_then(|source| source.source())
                .and_then(|source| source.downcast_ref::<BudgetedJsonError>()),
            Some(BudgetedJsonError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(page_budget.usage(), first_hit_usage);
    }

    #[test]
    fn oversized_response_hit_is_a_typed_internal_error() {
        let stamp = generation(b"oversized-response-hit");
        let (_directory, engine) = engine(vec![projected_reference("reference-a", -7)], stamp);
        let limits = ReferenceQueryLimits {
            max_hit_json_bytes: 1,
            ..REFERENCE_QUERY_LIMITS
        };

        let error = engine
            .references_with_limits(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1),
                limits,
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            ReferenceQueryError::ResponseHitTooLarge { .. }
        ));
        assert_eq!(error.api_code(), ApiErrorCode::Internal);
    }

    #[test]
    fn response_object_limit_is_checked_before_object_materialization() {
        let mut projected = projected_reference("reference-a", -7);
        projected.fact.resolution = ReferenceResolutionProjection::Ambiguous {
            candidates: vec![source_address(-99); MAX_REFERENCE_OBJECTS_PER_HIT],
        };

        let error = reference_hit(
            stored_reference(projected),
            GenerationStorageContract::CurrentV2,
        )
        .unwrap_err();

        assert!(matches!(
            &error,
            ReferenceQueryError::ResponseHitObjectLimitExceeded { .. }
        ));
        assert_eq!(error.api_code(), ApiErrorCode::Internal);
    }

    #[test]
    fn response_hit_page_budget_truncates_deterministically() {
        let stamp = generation(b"response-page-budget");
        let documents = vec![
            projected_reference("reference-a", -7),
            projected_reference("reference-b", -8),
        ];
        let (_directory, engine) = engine(documents, stamp);
        let baseline = engine
            .references(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 1),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let first_hit_bytes = reference_hit_json_bytes(&baseline.hits[0]).unwrap();
        let limits = ReferenceQueryLimits {
            max_page_hit_json_bytes: first_hit_bytes + 2,
            ..REFERENCE_QUERY_LIMITS
        };

        let first = engine
            .references_with_limits(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 2),
                limits,
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(first.hits.len(), 1);
        assert_eq!(first.hits[0].stable_id, "reference-a");
        assert!(first.coverage.truncated);
        let cursor = first.coverage.next_cursor.unwrap();

        let second = engine
            .references_with_limits(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 2).with_cursor(cursor),
                limits,
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(second.hits.len(), 1);
        assert_eq!(second.hits[0].stable_id, "reference-b");
        assert!(!second.coverage.truncated);
        assert!(second.coverage.next_cursor.is_none());
    }
}
