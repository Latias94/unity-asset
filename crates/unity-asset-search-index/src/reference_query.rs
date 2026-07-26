use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, TryReserveError, btree_map::Entry};
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tantivy::collector::{Collector, Count, SegmentCollector};
use tantivy::columnar::StrColumn;
use tantivy::query::TermQuery;
use tantivy::schema::{Field, IndexRecordOption, Value as _};
use tantivy::{DocAddress, DocId, IndexReader, Score, TantivyDocument, Term};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, Diagnostic, DiagnosticError, DigestV1, FieldPath, FieldPathError,
    FieldPathSegment, ObjectAddress, SourceLocator, arc_slice_allocation_bytes,
    string_allocation_bytes, vec_allocation_bytes,
};

use crate::analysis::{
    GuidProjection, RawReferenceProjection, ReferenceProjectionFact, ReferenceResolutionProjection,
};
use crate::contract::{
    ApiErrorCode, Location, MAX_REFERENCE_RESPONSE_DIAGNOSTIC_JSON_BYTES,
    MAX_REFERENCE_RESPONSE_DIAGNOSTICS, ReferenceContext, ReferenceCoverage, ReferenceCursor,
    ReferenceDiagnosticCoverage, ReferenceDirection, ReferenceHit, ReferenceObject,
    ReferenceRequest, ReferenceSelector, ReferencesResponse,
};
use crate::generation::{GenerationStamp, SEARCH_GENERATION_CONTRACT_VERSION};
use crate::projection::{reference_guid_key, reference_object_key};
use crate::store::{
    REFERENCE_SCHEMA_VERSION, ReferenceProjectionFields, ReferenceProjectionReader,
};

pub(crate) const MAX_REFERENCE_QUERY_LIMIT: usize = 500;
// The writer's default fact cap is 1 MiB. Query-side limits independently defend reopened or
// corrupted projections and bound both decoded input and serialized page output.
const MAX_STORED_REFERENCE_JSON_FIELD_BYTES: usize = 1024 * 1024;
const MAX_STORED_REFERENCE_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_REFERENCE_PAGE_STORED_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_REFERENCE_OBJECTS_PER_HIT: usize = 1024;
const MAX_REFERENCE_HIT_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_REFERENCE_PAGE_HIT_JSON_BYTES: usize = 8 * 1024 * 1024;
const REFERENCE_CURSOR_BINDING_DOMAIN: &[u8] = b"unity-asset:reference-query:cursor-binding:v1\0";
const REFERENCE_CURSOR_BINDING_PREFIX: &str = "reference-query-v1:";

#[derive(Debug, Clone, Copy)]
struct ReferenceQueryLimits {
    max_stored_json_field_bytes: usize,
    max_stored_document_bytes: usize,
    max_page_stored_json_bytes: usize,
    max_hit_json_bytes: usize,
    max_page_hit_json_bytes: usize,
    max_response_diagnostics: usize,
    max_response_diagnostic_json_bytes: usize,
}

const REFERENCE_QUERY_LIMITS: ReferenceQueryLimits = ReferenceQueryLimits {
    max_stored_json_field_bytes: MAX_STORED_REFERENCE_JSON_FIELD_BYTES,
    max_stored_document_bytes: MAX_STORED_REFERENCE_DOCUMENT_BYTES,
    max_page_stored_json_bytes: MAX_REFERENCE_PAGE_STORED_JSON_BYTES,
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

    fn finish(self) -> (Vec<Diagnostic>, ReferenceDiagnosticCoverage) {
        let coverage = ReferenceDiagnosticCoverage {
            returned: self.values.len(),
            truncated: self.truncated,
            total: self.total,
            serialized_bytes: self.serialized_bytes,
            max_count: self.max_count,
            max_serialized_bytes: self.max_serialized_bytes,
        };
        (self.values, coverage)
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
    ) -> Result<ReferencesResponse, ReferenceQueryError> {
        self.references_with_limits(request, REFERENCE_QUERY_LIMITS)
    }

    fn references_with_limits(
        &self,
        mut request: ReferenceRequest,
        limits: ReferenceQueryLimits,
    ) -> Result<ReferencesResponse, ReferenceQueryError> {
        let started = Instant::now();
        validate_request(&request, &self.snapshot.generation)?;
        let (field, key) = selector_key(
            &mut request.selector,
            request.direction,
            self.snapshot.fields,
        )?;
        let query_binding = reference_query_binding(request.direction, &key);
        validate_cursor_query_binding(request.cursor.as_ref(), &query_binding)?;

        let searcher = self.snapshot.reader.searcher();
        let query = TermQuery::new(Term::from_field_text(field, &key), IndexRecordOption::Basic);
        let fetch_limit = request.limit + 1;
        let after_stable_id = request
            .cursor
            .as_ref()
            .map(|cursor| Arc::<str>::from(cursor.after_stable_id.as_str()));
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

        let request_limit_has_more = documents.len() > request.limit;
        if request_limit_has_more {
            documents.truncate(request.limit);
        }

        let mut hits = Vec::with_capacity(documents.len());
        let mut diagnostics = ResponseDiagnostics::new(&self.snapshot.completeness, limits)?;
        let mut stored_json_bytes = 0;
        let mut hit_json_bytes = 2;
        let mut byte_limit_has_more = false;
        let mut last_returned_stable_id = None;
        let mut store_readers = BTreeMap::new();
        for selected in &documents {
            let segment_ord = selected.address.segment_ord;
            let store_reader = match store_readers.entry(segment_ord) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let reader = searcher
                        .segment_reader(segment_ord)
                        .get_store_reader(1)
                        .map_err(tantivy::TantivyError::from)?;
                    entry.insert(reader)
                }
            };
            let stored_document_bytes = store_reader.get_document_bytes(selected.address.doc_id)?;
            if stored_document_bytes.len() > limits.max_stored_document_bytes {
                return Err(ReferenceQueryError::CorruptDocument {
                    stable_id: Some(selected.stable_id.clone()),
                    reason: format!(
                        "encoded stored document is {} bytes, exceeding the {}-byte materialization \
                         limit",
                        stored_document_bytes.len(),
                        limits.max_stored_document_bytes
                    ),
                });
            }
            let document: TantivyDocument = store_reader.get(selected.address.doc_id)?;
            let remaining_stored_json_bytes = limits
                .max_page_stored_json_bytes
                .saturating_sub(stored_json_bytes);
            let Some(decoded) = decode_stored_reference_document(
                &document,
                self.snapshot.fields,
                limits.max_stored_json_field_bytes,
                remaining_stored_json_bytes,
            )?
            else {
                byte_limit_has_more = true;
                break;
            };
            let mut stored = decoded.document;
            if stored.stable_id != selected.stable_id {
                return Err(ReferenceQueryError::CorruptDocument {
                    stable_id: Some(selected.stable_id.clone()),
                    reason: "stored stable ID differs from the fast-field stable ID".to_owned(),
                });
            }

            let fact_diagnostics = std::mem::take(&mut stored.fact.diagnostics);
            let hit = reference_hit(stored)?;
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

            stored_json_bytes += decoded.stored_json_bytes;
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
                stored_json_maximum: limits.max_page_stored_json_bytes,
                hit_json_maximum: limits.max_page_hit_json_bytes,
            });
        }

        let has_more = request_limit_has_more || byte_limit_has_more;
        let complete = self.snapshot.completeness.is_complete();
        let next_cursor = if has_more {
            last_returned_stable_id.map(|after_stable_id| ReferenceCursor {
                generation: self.snapshot.generation.generation,
                after_stable_id,
                query_binding: Some(query_binding),
            })
        } else {
            None
        };

        let (diagnostics, diagnostic_coverage) = diagnostics.finish();

        Ok(ReferencesResponse {
            contract_version: SEARCH_GENERATION_CONTRACT_VERSION,
            generation: self.snapshot.generation.clone(),
            request,
            took_ms: started.elapsed().as_millis(),
            coverage: ReferenceCoverage {
                complete,
                truncated: has_more || !complete,
                returned: hits.len(),
                total: complete.then_some(total),
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
    UnsupportedContractVersion {
        actual: u16,
        expected: u16,
    },
    InvalidLimit {
        actual: usize,
        maximum: usize,
    },
    EmptyGuid,
    InvalidGuid,
    EmptyCursor,
    CursorGenerationMismatch {
        cursor: crate::generation::SearchGenerationId,
        active: crate::generation::SearchGenerationId,
    },
    MissingCursorQueryBinding,
    InvalidCursorQueryBinding,
    CursorQueryMismatch,
    Index(tantivy::TantivyError),
    CorruptDocument {
        stable_id: Option<String>,
        reason: String,
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
        stored_json_maximum: usize,
        hit_json_maximum: usize,
    },
    ResponseDiagnostic {
        reason: String,
    },
}

impl ReferenceQueryError {
    pub(crate) const fn api_code(&self) -> ApiErrorCode {
        match self {
            Self::UnsupportedContractVersion { .. }
            | Self::InvalidLimit { .. }
            | Self::EmptyGuid
            | Self::InvalidGuid => ApiErrorCode::InvalidRequest,
            Self::EmptyCursor
            | Self::CursorGenerationMismatch { .. }
            | Self::MissingCursorQueryBinding
            | Self::InvalidCursorQueryBinding
            | Self::CursorQueryMismatch => ApiErrorCode::InvalidCursor,
            Self::Index(_)
            | Self::CorruptDocument { .. }
            | Self::ResponseHitTooLarge { .. }
            | Self::ResponseHitObjectLimitExceeded { .. }
            | Self::ResponsePageBudgetTooSmall { .. }
            | Self::ResponseDiagnostic { .. } => ApiErrorCode::Internal,
        }
    }
}

impl fmt::Display for ReferenceQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedContractVersion { actual, expected } => write!(
                formatter,
                "reference request contract version {actual} is unsupported; expected {expected}"
            ),
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
            Self::CursorGenerationMismatch { cursor, active } => write!(
                formatter,
                "reference cursor generation {cursor} does not match active generation {active}"
            ),
            Self::MissingCursorQueryBinding => {
                formatter.write_str("reference cursor is missing its query binding")
            }
            Self::InvalidCursorQueryBinding => {
                formatter.write_str("reference cursor query binding is malformed")
            }
            Self::CursorQueryMismatch => formatter.write_str(
                "reference cursor belongs to a different selector or reference direction",
            ),
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
                stored_json_maximum,
                hit_json_maximum,
            } => write!(
                formatter,
                "reference hit {stable_id:?} cannot fit within the page budgets of \
                 {stored_json_maximum} stored-JSON bytes and {hit_json_maximum} response-hit bytes"
            ),
            Self::ResponseDiagnostic { reason } => write!(
                formatter,
                "reference response diagnostic cannot be serialized for response budgeting: {reason}"
            ),
        }
    }
}

impl Error for ReferenceQueryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Index(error) => Some(error),
            _ => None,
        }
    }
}

impl From<tantivy::TantivyError> for ReferenceQueryError {
    fn from(error: tantivy::TantivyError) -> Self {
        Self::Index(error)
    }
}

fn validate_request(
    request: &ReferenceRequest,
    generation: &GenerationStamp,
) -> Result<(), ReferenceQueryError> {
    if request.contract_version != SEARCH_GENERATION_CONTRACT_VERSION {
        return Err(ReferenceQueryError::UnsupportedContractVersion {
            actual: request.contract_version,
            expected: SEARCH_GENERATION_CONTRACT_VERSION,
        });
    }
    if !(1..=MAX_REFERENCE_QUERY_LIMIT).contains(&request.limit) {
        return Err(ReferenceQueryError::InvalidLimit {
            actual: request.limit,
            maximum: MAX_REFERENCE_QUERY_LIMIT,
        });
    }
    if let Some(cursor) = &request.cursor {
        if cursor.after_stable_id.is_empty() {
            return Err(ReferenceQueryError::EmptyCursor);
        }
        if cursor.generation != generation.generation {
            return Err(ReferenceQueryError::CursorGenerationMismatch {
                cursor: cursor.generation,
                active: generation.generation,
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
    let Some(actual) = cursor.query_binding.as_deref() else {
        return Err(ReferenceQueryError::MissingCursorQueryBinding);
    };
    if !is_valid_cursor_query_binding(actual) {
        return Err(ReferenceQueryError::InvalidCursorQueryBinding);
    }
    if actual != expected {
        return Err(ReferenceQueryError::CursorQueryMismatch);
    }
    Ok(())
}

fn reference_query_binding(direction: ReferenceDirection, normalized_selector_key: &str) -> String {
    let mut identity = Vec::from(REFERENCE_CURSOR_BINDING_DOMAIN);
    identity.push(match direction {
        ReferenceDirection::Incoming => 0,
        ReferenceDirection::Outgoing => 1,
    });
    identity.extend_from_slice(normalized_selector_key.as_bytes());
    format!(
        "{REFERENCE_CURSOR_BINDING_PREFIX}{}",
        hex::encode(DigestV1::hash_bytes(&identity).as_bytes())
    )
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
    selector: &mut ReferenceSelector,
    direction: ReferenceDirection,
    fields: ReferenceProjectionFields,
) -> Result<(Field, String), ReferenceQueryError> {
    let field = match direction {
        ReferenceDirection::Incoming => fields.incoming_key(),
        ReferenceDirection::Outgoing => fields.outgoing_key(),
    };
    let key = match selector {
        ReferenceSelector::Object { address } => reference_object_key(address),
        ReferenceSelector::Guid { guid, file_id } => {
            let normalized = guid.trim().to_ascii_lowercase();
            if normalized.is_empty() {
                return Err(ReferenceQueryError::EmptyGuid);
            }
            if normalized.len() != 32 || !normalized.as_bytes().iter().all(u8::is_ascii_hexdigit) {
                return Err(ReferenceQueryError::InvalidGuid);
            }
            *guid = normalized;
            reference_guid_key(guid, *file_id)
        }
    };
    Ok((field, key))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StableReferenceDocument {
    stable_id: String,
    address: DocAddress,
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
        Ok(StableReferenceSegmentCollector {
            limit: self.limit,
            segment_ord: segment_local_id,
            stable_ids,
            after_stable_id: self.after_stable_id.clone(),
            scratch: String::new(),
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
    after_stable_id: Option<Arc<str>>,
    scratch: String,
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
struct StoredReferenceDocument {
    stable_id: String,
    source_path: String,
    source_kind: String,
    source_guid: Option<String>,
    source_object: Option<ObjectAddress>,
    source_file_id: Option<i64>,
    source_class_id: Option<i32>,
    fact: ReferenceProjectionFact,
}

#[derive(Debug)]
struct DecodedStoredReferenceDocument {
    document: StoredReferenceDocument,
    stored_json_bytes: usize,
}

fn decode_stored_reference_document(
    document: &TantivyDocument,
    fields: ReferenceProjectionFields,
    max_json_field_bytes: usize,
    remaining_page_json_bytes: usize,
) -> Result<Option<DecodedStoredReferenceDocument>, ReferenceQueryError> {
    let schema_version = required_u64(document, fields.schema_version(), "schema_version", None)?;
    if schema_version != u64::from(REFERENCE_SCHEMA_VERSION) {
        return Err(ReferenceQueryError::CorruptDocument {
            stable_id: None,
            reason: format!(
                "stored schema_version {schema_version} does not match {REFERENCE_SCHEMA_VERSION}"
            ),
        });
    }
    let stable_id = required_text(document, fields.stable_id(), "stable_id", None)?;
    if stable_id.is_empty() {
        return Err(corrupt_field(None, "stable_id", "is empty"));
    }
    let source_path = required_text(
        document,
        fields.source_path(),
        "source_path",
        Some(stable_id),
    )?;
    let source_kind = required_text(
        document,
        fields.source_kind(),
        "source_kind",
        Some(stable_id),
    )?;
    let source_guid = optional_text(
        document,
        fields.source_guid(),
        "source_guid",
        Some(stable_id),
    )?;
    let source_object_json = optional_text(
        document,
        fields.source_object_json(),
        "source_object_json",
        Some(stable_id),
    )?;
    let source_file_id = optional_i64(
        document,
        fields.source_file_id(),
        "source_file_id",
        Some(stable_id),
    )?;
    let source_class_id = optional_i64(
        document,
        fields.source_class_id(),
        "source_class_id",
        Some(stable_id),
    )?
    .map(|value| {
        i32::try_from(value).map_err(|_| ReferenceQueryError::CorruptDocument {
            stable_id: Some(stable_id.to_owned()),
            reason: format!("source_class_id {value} does not fit i32"),
        })
    })
    .transpose()?;
    let fact_json = required_text(document, fields.fact_json(), "fact_json", Some(stable_id))?;

    if let Some(source_object_json) = source_object_json {
        validate_stored_json_field_size(
            stable_id,
            "source_object_json",
            source_object_json,
            max_json_field_bytes,
        )?;
    }
    validate_stored_json_field_size(stable_id, "fact_json", fact_json, max_json_field_bytes)?;
    let stored_json_bytes = source_object_json
        .map_or(0, str::len)
        .checked_add(fact_json.len())
        .ok_or_else(|| ReferenceQueryError::CorruptDocument {
            stable_id: Some(stable_id.to_owned()),
            reason: "stored JSON byte count overflows usize".to_owned(),
        })?;
    if stored_json_bytes > remaining_page_json_bytes {
        return Ok(None);
    }

    let source_object = source_object_json
        .map(|encoded| {
            serde_json::from_str(encoded).map_err(|error| ReferenceQueryError::CorruptDocument {
                stable_id: Some(stable_id.to_owned()),
                reason: format!("source_object_json is invalid: {error}"),
            })
        })
        .transpose()?;
    let fact: ReferenceProjectionFact =
        serde_json::from_str(fact_json).map_err(|error| ReferenceQueryError::CorruptDocument {
            stable_id: Some(stable_id.to_owned()),
            reason: format!("fact_json is invalid: {error}"),
        })?;

    if fact.source_object != source_object
        || fact.source_file_id != source_file_id
        || fact.source_class_id != source_class_id
    {
        return Err(ReferenceQueryError::CorruptDocument {
            stable_id: Some(stable_id.to_owned()),
            reason: "stored source identity differs from fact_json".to_owned(),
        });
    }

    Ok(Some(DecodedStoredReferenceDocument {
        document: StoredReferenceDocument {
            stable_id: stable_id.to_owned(),
            source_path: source_path.to_owned(),
            source_kind: source_kind.to_owned(),
            source_guid: source_guid.map(str::to_owned),
            source_object,
            source_file_id,
            source_class_id,
            fact,
        },
        stored_json_bytes,
    }))
}

fn validate_stored_json_field_size(
    stable_id: &str,
    field_name: &str,
    value: &str,
    maximum: usize,
) -> Result<(), ReferenceQueryError> {
    if value.len() > maximum {
        return Err(ReferenceQueryError::CorruptDocument {
            stable_id: Some(stable_id.to_owned()),
            reason: format!(
                "stored field {field_name:?} is {} bytes, exceeding the {maximum}-byte decode limit",
                value.len()
            ),
        });
    }
    Ok(())
}

fn required_text<'a>(
    document: &'a TantivyDocument,
    field: Field,
    field_name: &str,
    stable_id: Option<&str>,
) -> Result<&'a str, ReferenceQueryError> {
    let mut values = document.get_all(field);
    let Some(value) = values.next() else {
        return Err(corrupt_field(stable_id, field_name, "is missing"));
    };
    if values.next().is_some() {
        return Err(corrupt_field(
            stable_id,
            field_name,
            "has multiple stored values",
        ));
    }
    value
        .as_str()
        .ok_or_else(|| corrupt_field(stable_id, field_name, "is not text"))
}

fn optional_text<'a>(
    document: &'a TantivyDocument,
    field: Field,
    field_name: &str,
    stable_id: Option<&str>,
) -> Result<Option<&'a str>, ReferenceQueryError> {
    let mut values = document.get_all(field);
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(corrupt_field(
            stable_id,
            field_name,
            "has multiple stored values",
        ));
    }
    value
        .as_str()
        .map(Some)
        .ok_or_else(|| corrupt_field(stable_id, field_name, "is not text"))
}

fn optional_i64(
    document: &TantivyDocument,
    field: Field,
    field_name: &str,
    stable_id: Option<&str>,
) -> Result<Option<i64>, ReferenceQueryError> {
    let mut values = document.get_all(field);
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(corrupt_field(
            stable_id,
            field_name,
            "has multiple stored values",
        ));
    }
    value
        .as_i64()
        .map(Some)
        .ok_or_else(|| corrupt_field(stable_id, field_name, "is not i64"))
}

fn required_u64(
    document: &TantivyDocument,
    field: Field,
    field_name: &str,
    stable_id: Option<&str>,
) -> Result<u64, ReferenceQueryError> {
    let mut values = document.get_all(field);
    let Some(value) = values.next() else {
        return Err(corrupt_field(stable_id, field_name, "is missing"));
    };
    if values.next().is_some() {
        return Err(corrupt_field(
            stable_id,
            field_name,
            "has multiple stored values",
        ));
    }
    value
        .as_u64()
        .ok_or_else(|| corrupt_field(stable_id, field_name, "is not u64"))
}

fn corrupt_field(stable_id: Option<&str>, field_name: &str, problem: &str) -> ReferenceQueryError {
    ReferenceQueryError::CorruptDocument {
        stable_id: stable_id.map(str::to_owned),
        reason: format!("stored field {field_name:?} {problem}"),
    }
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

fn reference_hit(stored: StoredReferenceDocument) -> Result<ReferenceHit, ReferenceQueryError> {
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
    let mut raw_object = raw_reference_object(&stored)?;
    raw_object
        .field_hints
        .push(resolution_hint(&stored.fact.resolution).to_owned());
    let mut objects = vec![raw_object];
    objects.extend(resolution_objects(&stored)?);

    Ok(ReferenceHit {
        source_path: stored.source_path.clone(),
        source_kind: stored.source_kind,
        stable_id: stored.stable_id,
        location: Location {
            path: stored.source_path,
            guid: stored.source_guid,
            file_id: stored.source_file_id,
            class_id: stored.source_class_id,
        },
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
    stored: &StoredReferenceDocument,
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
                reference_object_key(address)
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
                location: Location {
                    path,
                    guid: target_guid
                        .or_else(|| target_address.as_ref().and(stored.source_guid.clone())),
                    file_id: Some(*path_id),
                    class_id: None,
                },
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
                    ObjectAddress::yaml(source.source_locator().clone(), file_id.to_string()).ok()
                }
                _ => None,
            };
            let stable_id = if let Some(address) = &target_address {
                reference_object_key(address)
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
                location: Location {
                    path: stored.source_path.clone(),
                    guid: target_guid
                        .or_else(|| target_address.as_ref().and(stored.source_guid.clone())),
                    file_id: *file_id,
                    class_id: None,
                },
                object_name: None,
                hierarchy_path: None,
                field_hints,
            })
        }
    }
}

fn resolution_objects(
    stored: &StoredReferenceDocument,
) -> Result<Vec<ReferenceObject>, ReferenceQueryError> {
    let mut objects = Vec::new();
    match &stored.fact.resolution {
        ReferenceResolutionProjection::Null
        | ReferenceResolutionProjection::Missing { target: None }
        | ReferenceResolutionProjection::Invalid => {}
        ReferenceResolutionProjection::Resolved { target } => {
            objects.push(address_object(target, "resolution.resolved"));
        }
        ReferenceResolutionProjection::Missing {
            target: Some(target),
        } => {
            objects.push(address_object(target, "resolution.missing"));
        }
        ReferenceResolutionProjection::Ambiguous { candidates } => {
            objects.extend(
                candidates
                    .iter()
                    .map(|target| address_object(target, "resolution.ambiguous")),
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
                location: Location {
                    path: locator_path(source),
                    guid: None,
                    file_id: None,
                    class_id: None,
                },
                object_name: None,
                hierarchy_path: None,
                field_hints: vec!["resolution.unloaded".to_owned()],
            });
        }
        ReferenceResolutionProjection::Unloaded { source: None } => {}
    }
    Ok(objects)
}

fn address_object(address: &ObjectAddress, resolution: &str) -> ReferenceObject {
    let file_id = address.binary_path_id().or_else(|| {
        address
            .yaml_anchor()
            .and_then(|anchor| anchor.parse::<i64>().ok())
    });
    ReferenceObject {
        doc_file_id: file_id,
        doc_class_id: None,
        stable_id: reference_object_key(address),
        location: Location {
            path: locator_path(address.source_locator()),
            guid: None,
            file_id,
            class_id: None,
        },
        object_name: None,
        hierarchy_path: None,
        field_hints: vec![resolution.to_owned()],
    }
}

fn locator_path(locator: &SourceLocator) -> String {
    let mut path = locator.root_alias().as_str().to_owned();
    for step in locator.members() {
        path.push_str("::");
        path.push_str(step.container().tag());
        path.push(':');
        path.push_str(step.member().name());
        let occurrence = step.member().same_name_occurrence();
        if occurrence != 0 {
            path.push('@');
            path.push_str(&occurrence.to_string());
        }
    }
    path
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
    use tempfile::tempdir;
    use unity_asset_core::{
        AssetLoadLimits, AssetLoadUsage, DiagnosticSeverity, FieldPath, WorkspaceId,
        WorkspaceRevision,
    };

    use super::*;
    use crate::analysis::{BinaryExternalProjection, ReferenceDependencyKey};
    use crate::generation::SearchGenerationId;
    use crate::projection::{GenerationProjection, ProjectionMetrics, ReferenceDocument};
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

    fn stored_reference(projected: ReferenceDocument) -> StoredReferenceDocument {
        StoredReferenceDocument {
            stable_id: projected.stable_id,
            source_path: projected.source_path,
            source_kind: projected.source_kind,
            source_guid: projected.source_guid,
            source_object: projected.source_object,
            source_file_id: projected.source_file_id,
            source_class_id: projected.source_class_id,
            fact: projected.fact,
        }
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
        let readers = ProjectionReaders::open(directory.path()).unwrap();
        let snapshot = ReferenceQuerySnapshot::new(stamp, readers.references(), completeness);
        (directory, ReferenceQueryEngine::new(Arc::new(snapshot)))
    }

    #[test]
    fn completeness_deduplicates_borrowed_diagnostics_and_accounts_retained_storage() {
        let address = ObjectAddress::yaml(
            SourceLocator::archive_member("Assets/Archive.zip", "Nested.asset").unwrap(),
            "12345",
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
        assert_ne!(
            retained_address.yaml_anchor().unwrap().as_ptr(),
            input_address.yaml_anchor().unwrap().as_ptr()
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
            )
            .unwrap();
        assert_eq!(exact.diagnostics.len(), 2);
        assert_eq!(
            exact.diagnostic_coverage,
            ReferenceDiagnosticCoverage {
                returned: 2,
                truncated: false,
                total: Some(2),
                serialized_bytes: exact_bytes,
                max_count: 2,
                max_serialized_bytes: exact_bytes,
            }
        );
        assert_eq!(
            serde_json::to_vec(&exact.diagnostics).unwrap().len(),
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
    fn uppercase_guid_and_negative_ids_round_trip_without_source_io() {
        let stamp = generation(b"one");
        let (_directory, engine) = engine(vec![projected_reference("reference-a", -7)], stamp);
        let response = engine
            .references(ReferenceRequest::incoming_guid(
                GUID.to_ascii_uppercase(),
                Some(-99),
                10,
            ))
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
    fn malformed_guid_is_rejected_instead_of_reported_as_no_matches() {
        let stamp = generation(b"invalid-guid");
        let (_directory, engine) = engine(Vec::new(), stamp);

        let error = engine
            .references(ReferenceRequest::incoming_guid("not-a-guid", None, 10))
            .unwrap_err();

        assert!(matches!(error, ReferenceQueryError::InvalidGuid));
        assert_eq!(error.api_code(), ApiErrorCode::InvalidRequest);
    }

    #[test]
    fn incoming_and_outgoing_use_their_respective_key_fields() {
        let document = projected_reference("reference-a", -7);
        let source = document.source_object.clone().unwrap();
        let stamp = generation(b"two");
        let (_directory, engine) = engine(vec![document], stamp);

        let incoming = engine
            .references(ReferenceRequest::incoming_guid(GUID, None, 10))
            .unwrap();
        let outgoing = engine
            .references(ReferenceRequest {
                contract_version: SEARCH_GENERATION_CONTRACT_VERSION,
                direction: ReferenceDirection::Outgoing,
                selector: ReferenceSelector::Object { address: source },
                limit: 10,
                cursor: None,
            })
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
            .references(ReferenceRequest::incoming_guid(
                GUID.to_ascii_uppercase(),
                Some(-99),
                1,
            ))
            .unwrap();
        assert_eq!(first.hits[0].stable_id, "reference-a");
        assert_eq!(first.coverage.total, Some(3));
        assert!(first.coverage.truncated);
        let cursor = first.coverage.next_cursor.unwrap();
        assert_eq!(cursor.generation, stamp.generation);
        assert!(is_valid_cursor_query_binding(
            cursor.query_binding.as_deref().unwrap()
        ));
        let cursor: ReferenceCursor =
            serde_json::from_value(serde_json::to_value(cursor).unwrap()).unwrap();

        let second = engine
            .references(ReferenceRequest {
                contract_version: SEARCH_GENERATION_CONTRACT_VERSION,
                direction: ReferenceDirection::Incoming,
                selector: ReferenceSelector::Guid {
                    guid: GUID.to_owned(),
                    file_id: Some(-99),
                },
                limit: 1,
                cursor: Some(cursor),
            })
            .unwrap();
        assert_eq!(second.hits[0].stable_id, "reference-b");
    }

    #[test]
    fn cursor_from_another_generation_is_a_typed_error() {
        let stamp = generation(b"active");
        let (_directory, engine) = engine(vec![projected_reference("reference-a", -7)], stamp);
        let error = engine
            .references(ReferenceRequest {
                contract_version: SEARCH_GENERATION_CONTRACT_VERSION,
                direction: ReferenceDirection::Incoming,
                selector: ReferenceSelector::Guid {
                    guid: GUID.to_owned(),
                    file_id: None,
                },
                limit: 10,
                cursor: Some(ReferenceCursor {
                    generation: generation(b"old").generation,
                    after_stable_id: "reference-a".to_owned(),
                    query_binding: None,
                }),
            })
            .unwrap_err();

        assert!(matches!(
            &error,
            ReferenceQueryError::CursorGenerationMismatch { .. }
        ));
        assert_eq!(error.api_code(), ApiErrorCode::InvalidCursor);
    }

    #[test]
    fn legacy_cursor_without_query_binding_is_a_typed_error() {
        let stamp = generation(b"legacy-cursor");
        let legacy_cursor: ReferenceCursor = serde_json::from_value(serde_json::json!({
            "generation": stamp.generation,
            "after_stable_id": "reference-a",
        }))
        .unwrap();
        assert_eq!(legacy_cursor.query_binding, None);
        let (_directory, engine) = engine(vec![projected_reference("reference-a", -7)], stamp);

        let error = engine
            .references(ReferenceRequest::incoming_guid(GUID, None, 10).with_cursor(legacy_cursor))
            .unwrap_err();

        assert!(matches!(
            &error,
            ReferenceQueryError::MissingCursorQueryBinding
        ));
        assert_eq!(error.api_code(), ApiErrorCode::InvalidCursor);
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
            .references(ReferenceRequest::incoming_guid(GUID, Some(-99), 1))
            .unwrap()
            .coverage
            .next_cursor
            .unwrap();

        let error = engine
            .references(ReferenceRequest::outgoing_guid(GUID, Some(-99), 1).with_cursor(cursor))
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
            .references(ReferenceRequest::incoming_guid(GUID, Some(-99), 1))
            .unwrap()
            .coverage
            .next_cursor
            .unwrap();

        let error = engine
            .references(ReferenceRequest::incoming_guid(GUID, None, 1).with_cursor(cursor))
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
            .references(ReferenceRequest::incoming_guid(GUID, Some(-99), 1))
            .unwrap()
            .coverage
            .next_cursor
            .unwrap();
        cursor.query_binding = Some("reference-query-v1:not-a-digest".to_owned());

        let error = engine
            .references(ReferenceRequest::incoming_guid(GUID, Some(-99), 1).with_cursor(cursor))
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
            .references(ReferenceRequest::incoming_guid(GUID, None, 10))
            .unwrap();

        assert!(!response.coverage.complete);
        assert!(response.coverage.truncated);
        assert_eq!(response.coverage.total, None);
    }

    #[test]
    fn corrupt_stored_fact_json_is_not_silently_ignored() {
        let directory = tempdir().unwrap();
        ProjectionStore::build(directory.path(), &projection(Vec::new())).unwrap();
        let readers = ProjectionReaders::open(directory.path()).unwrap();
        let fields = *readers.references().fields();
        let mut document = TantivyDocument::default();
        document.add_u64(fields.schema_version(), u64::from(REFERENCE_SCHEMA_VERSION));
        document.add_text(fields.stable_id(), "corrupt-reference");
        document.add_text(fields.source_path(), "Assets/Corrupt.asset");
        document.add_text(fields.source_kind(), "SerializedAsset");
        document.add_text(fields.fact_json(), "{not-json");

        let error = decode_stored_reference_document(
            &document,
            fields,
            MAX_STORED_REFERENCE_JSON_FIELD_BYTES,
            MAX_REFERENCE_PAGE_STORED_JSON_BYTES,
        )
        .unwrap_err();
        assert!(matches!(error, ReferenceQueryError::CorruptDocument { .. }));
    }

    #[test]
    fn stored_json_limit_is_checked_before_fact_decoding() {
        let directory = tempdir().unwrap();
        ProjectionStore::build(directory.path(), &projection(Vec::new())).unwrap();
        let readers = ProjectionReaders::open(directory.path()).unwrap();
        let fields = *readers.references().fields();
        let mut document = TantivyDocument::default();
        document.add_u64(fields.schema_version(), u64::from(REFERENCE_SCHEMA_VERSION));
        document.add_text(fields.stable_id(), "oversized-reference");
        document.add_text(fields.source_path(), "Assets/Oversized.asset");
        document.add_text(fields.source_kind(), "SerializedAsset");
        document.add_text(fields.fact_json(), "{not-json");

        let error = decode_stored_reference_document(&document, fields, 4, usize::MAX).unwrap_err();

        match error {
            ReferenceQueryError::CorruptDocument { reason, .. } => {
                assert!(reason.contains("decode limit"));
                assert!(!reason.contains("is invalid"));
            }
            other => panic!("expected corrupt stored JSON, got {other:?}"),
        }
    }

    #[test]
    fn stored_document_limit_is_checked_before_materialization() {
        let stamp = generation(b"stored-document-limit");
        let (_directory, engine) = engine(vec![projected_reference("reference-a", -7)], stamp);
        let limits = ReferenceQueryLimits {
            max_stored_document_bytes: 1,
            ..REFERENCE_QUERY_LIMITS
        };

        let error = engine
            .references_with_limits(ReferenceRequest::incoming_guid(GUID, Some(-99), 1), limits)
            .unwrap_err();

        assert_eq!(error.api_code(), ApiErrorCode::Internal);
        match error {
            ReferenceQueryError::CorruptDocument { reason, .. } => {
                assert!(reason.contains("materialization limit"));
            }
            other => panic!("expected oversized stored document, got {other:?}"),
        }
    }

    #[test]
    fn stored_json_page_budget_truncates_with_a_bound_cursor() {
        let first_document = projected_reference("reference-a", -7);
        let stored_json_bytes = serde_json::to_string(&first_document.fact).unwrap().len()
            + serde_json::to_string(first_document.source_object.as_ref().unwrap())
                .unwrap()
                .len();
        let stamp = generation(b"stored-json-page-budget");
        let documents = vec![first_document, projected_reference("reference-b", -8)];
        let (_directory, engine) = engine(documents, stamp);
        let limits = ReferenceQueryLimits {
            max_page_stored_json_bytes: stored_json_bytes,
            ..REFERENCE_QUERY_LIMITS
        };

        let first = engine
            .references_with_limits(ReferenceRequest::incoming_guid(GUID, Some(-99), 2), limits)
            .unwrap();

        assert_eq!(first.hits.len(), 1);
        assert_eq!(first.hits[0].stable_id, "reference-a");
        assert!(first.coverage.truncated);
        assert!(first.coverage.next_cursor.is_some());
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
            .references_with_limits(ReferenceRequest::incoming_guid(GUID, Some(-99), 1), limits)
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

        let error = reference_hit(stored_reference(projected)).unwrap_err();

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
            .references(ReferenceRequest::incoming_guid(GUID, Some(-99), 1))
            .unwrap();
        let first_hit_bytes = reference_hit_json_bytes(&baseline.hits[0]).unwrap();
        let limits = ReferenceQueryLimits {
            max_page_hit_json_bytes: first_hit_bytes + 2,
            ..REFERENCE_QUERY_LIMITS
        };

        let first = engine
            .references_with_limits(ReferenceRequest::incoming_guid(GUID, Some(-99), 2), limits)
            .unwrap();
        assert_eq!(first.hits.len(), 1);
        assert_eq!(first.hits[0].stable_id, "reference-a");
        assert!(first.coverage.truncated);
        let cursor = first.coverage.next_cursor.unwrap();

        let second = engine
            .references_with_limits(
                ReferenceRequest::incoming_guid(GUID, Some(-99), 2).with_cursor(cursor),
                limits,
            )
            .unwrap();
        assert_eq!(second.hits.len(), 1);
        assert_eq!(second.hits[0].stable_id, "reference-b");
        assert!(!second.coverage.truncated);
        assert!(second.coverage.next_cursor.is_none());
    }
}
