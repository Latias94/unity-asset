use std::cmp::{Ordering as CmpOrdering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, btree_map::Entry};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::StrColumn;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, EnableScoring, FuzzyTermQuery, PhrasePrefixQuery,
    PhraseQuery, Query, RegexQuery, TermQuery, Weight,
};
use tantivy::schema::{Field, Schema, Value as _};
use tantivy::{DocAddress, DocId, DocSet, IndexReader, Score, TantivyDocument, Term};
use unity_asset_core::{
    AssetLoadBudget, DigestV1Builder, arc_slice_allocation_bytes, string_allocation_bytes,
    vec_allocation_bytes,
};
use unity_asset_search_core::{
    CandidateFacts, CandidateField, MatchField, MatchKind, QuerySpec, RetrievalEvidence,
    RetrievalStage, RetrievalTerm, SearchDiagnostic, SearchKind, SearchLimits, SearchPolicy,
    SearchRequest, normalize_for_match, to_terms, try_to_terms,
};
use unity_asset_search_protocol::{
    FuzzyWorkUsageV1, HighlightRangeV1, Location, MAX_SEARCH_HITS_JSON_BYTES, MatchCountV1,
    SEARCH_PROTOCOL_REVISION, SearchDiagnosticV1, SearchHit, SearchResponse, SuggestResponse,
    ValidateContract,
};

use crate::generation::GenerationStamp;
use crate::wire;

const MAX_MATCHED_HIERARCHY_PATHS: usize = 6;
const MAX_MATCHED_SCRIPT_SYMBOLS: usize = 12;
const MAX_STORED_CANDIDATE_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;
// Mirror the generation analyzer caps so a reopened or corrupt index cannot bypass the writer
// contract at query time.
const MAX_STORED_HIERARCHY_PATHS: usize = 100_000;
const MAX_STORED_SCRIPT_SYMBOLS: usize = 4_096;
/// Tantivy fields consumed by the generation-bound query engine.
///
/// The generation writer owns the schema, while this type is the single validation boundary used
/// before a reader becomes queryable. Hierarchy paths and script symbols are stored projections;
/// query execution never reopens source assets to enrich a hit.
#[derive(Debug, Clone)]
pub(crate) struct SearchQueryFields {
    pub(crate) id: Field,
    pub(crate) guid: Field,
    pub(crate) path: Field,
    pub(crate) path_filter: Field,
    pub(crate) path_terms: Field,
    pub(crate) name: Field,
    pub(crate) name_terms: Field,
    pub(crate) kind: Field,
    pub(crate) kind_filter: Field,
    pub(crate) kind_terms: Field,
    pub(crate) content_terms: Field,
    pub(crate) container_source_path: Field,
    pub(crate) hierarchy_paths: Field,
    pub(crate) script_symbols: Field,
}

impl SearchQueryFields {
    pub(crate) fn from_schema(schema: &Schema) -> Result<Self> {
        let id = schema.get_field("id")?;
        if !schema.get_field_entry(id).is_fast() {
            return Err(anyhow!("search index field `id` must be a fast field"));
        }
        Ok(Self {
            id,
            guid: schema.get_field("guid")?,
            path: schema.get_field("path")?,
            path_filter: schema.get_field("path_filter")?,
            path_terms: schema.get_field("path_terms")?,
            name: schema.get_field("name")?,
            name_terms: schema.get_field("name_terms")?,
            kind: schema.get_field("kind")?,
            kind_filter: schema.get_field("kind_filter")?,
            kind_terms: schema.get_field("kind_terms")?,
            content_terms: schema.get_field("content_terms")?,
            container_source_path: schema.get_field("container_source_path")?,
            hierarchy_paths: schema.get_field("hierarchy_paths")?,
            script_symbols: schema.get_field("script_symbols")?,
        })
    }
}

/// One immutable reader view and its logical generation identity.
///
/// A query captures this `Arc` once. Switching the active generation therefore cannot mix a
/// response stamp, Tantivy reader, or suggestion state from different revisions.
#[derive(Clone)]
pub(crate) struct QuerySnapshot {
    generation: GenerationStamp,
    reader: IndexReader,
    fields: SearchQueryFields,
    path_suggestions: PathSuggestionIndex,
}

impl QuerySnapshot {
    pub(crate) fn new<I, P>(
        generation: GenerationStamp,
        reader: IndexReader,
        fields: SearchQueryFields,
        paths: I,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<str>,
    {
        Ok(Self {
            generation,
            reader,
            fields,
            path_suggestions: PathSuggestionIndex::new(paths, budget)
                .context("build generation path suggestion index")?,
        })
    }
}

/// Generation-bound parent directories used to answer suggestions without scanning raw paths.
///
/// Each source path contributes at most one string no longer than itself. Construction sorts and
/// deduplicates those directories in `O(n log n)` time; querying derives direct children and skips
/// their sorted subtree.
#[derive(Clone)]
struct PathSuggestionIndex {
    sorted_directories: Arc<[String]>,
}

impl PathSuggestionIndex {
    fn new<I, P>(paths: I, budget: &mut AssetLoadBudget) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<str>,
    {
        let mut directories = Vec::<String>::new();
        for path in paths {
            budget
                .check_entries(1)
                .context("preflight path suggestion source entry")?;

            let path = path.as_ref();
            let directory = path.rfind('/').map_or(path, |position| &path[..=position]);
            let directory_bytes = string_allocation_bytes(directory.len())
                .context("calculate retained path suggestion directory bytes")?;
            let additional_capacity = if directories.len() == directories.capacity() {
                if directories.capacity() == 0 {
                    1
                } else {
                    directories
                        .capacity()
                        .checked_mul(2)
                        .and_then(|capacity| capacity.checked_sub(directories.len()))
                        .ok_or_else(|| anyhow!("path suggestion vector capacity overflow"))?
                }
            } else {
                0
            };
            let vector_bytes = if additional_capacity == 0 {
                0
            } else {
                vec_allocation_bytes::<String>(additional_capacity)
                    .context("calculate retained path suggestion vector growth")?
            };
            let retained_bytes = directory_bytes
                .checked_add(vector_bytes)
                .ok_or_else(|| anyhow!("path suggestion directory allocation size overflow"))?;
            budget
                .check_members(1)
                .context("preflight retained path suggestion member")?;
            budget
                .check_bytes(retained_bytes)
                .context("preflight retained path suggestion directory")?;
            budget
                .consume_entries(1)
                .context("charge path suggestion source entry")?;
            budget
                .consume_members(1)
                .context("charge retained path suggestion member")?;
            budget
                .consume_bytes(retained_bytes)
                .context("charge retained path suggestion directory")?;

            if additional_capacity != 0 {
                directories
                    .try_reserve_exact(additional_capacity)
                    .map_err(|error| {
                        anyhow!(
                            "reserve {additional_capacity} entries for path suggestions: {error}"
                        )
                    })?;
            }
            let mut owned_directory = String::new();
            owned_directory
                .try_reserve_exact(directory.len())
                .map_err(|error| {
                    anyhow!(
                        "reserve {} bytes for retained path suggestion directory: {error}",
                        directory.len()
                    )
                })?;
            owned_directory.push_str(directory);
            directories.push(owned_directory);
        }

        directories.sort_unstable();
        let unique_count = if directories.is_empty() {
            0
        } else {
            1 + directories
                .windows(2)
                .filter(|pair| pair[0] != pair[1])
                .count()
        };
        let unique_vector_bytes = vec_allocation_bytes::<String>(unique_count)
            .context("calculate unique path suggestion vector backing")?;
        budget
            .check_bytes(unique_vector_bytes)
            .context("preflight unique path suggestion vector backing")?;
        budget
            .consume_bytes(unique_vector_bytes)
            .context("charge unique path suggestion vector backing")?;
        let mut unique_directories = Vec::new();
        unique_directories
            .try_reserve_exact(unique_count)
            .map_err(|error| anyhow!("reserve unique path suggestions: {error}"))?;
        for directory in directories {
            if unique_directories
                .last()
                .is_none_or(|previous: &String| previous != &directory)
            {
                unique_directories.push(directory);
            }
        }
        debug_assert_eq!(unique_directories.len(), unique_count);

        let directory_count = unique_directories.len();
        let arc_bytes = arc_slice_allocation_bytes::<String>(directory_count)
            .context("calculate final path suggestion Arc slice bytes")?;
        budget
            .check_bytes(arc_bytes)
            .context("preflight final path suggestion backing")?;
        budget
            .consume_bytes(arc_bytes)
            .context("charge final path suggestion backing")?;

        Ok(Self {
            sorted_directories: Arc::from(unique_directories.into_boxed_slice()),
        })
    }

    fn suggest(&self, raw_prefix: &str, limit: usize) -> Vec<String> {
        suggest_in_paths(&self.sorted_directories, raw_prefix, limit)
    }
}

#[derive(Clone)]
pub(crate) struct QueryEngine {
    snapshot: Arc<QuerySnapshot>,
    policy: SearchPolicy,
}

impl QueryEngine {
    pub(crate) fn new(snapshot: Arc<QuerySnapshot>) -> Self {
        Self {
            snapshot,
            policy: SearchPolicy::default(),
        }
    }

    pub(crate) fn search(&self, request: SearchRequest) -> Result<SearchResponse> {
        let start = Instant::now();
        let SearchRequest { query, limit } = request;
        let response_query = query;
        let query = trim_owned_query(response_query.clone());
        if query.len() > self.policy.limits.max_query_bytes {
            let mut outcome = self
                .policy
                .prepare(SearchRequest::new(String::new(), limit))
                .execute(Vec::new());
            outcome.diagnostics.clear();
            outcome
                .diagnostics
                .push(SearchDiagnostic::QueryByteLimitExceeded {
                    actual: query.len(),
                    limit: self.policy.limits.max_query_bytes,
                });
            return validated_search_response(SearchResponse {
                protocol_revision: SEARCH_PROTOCOL_REVISION,
                generation: wire::generation_stamp(&self.snapshot.generation),
                query_policy_id: wire::query_policy_id(),
                query: response_query,
                took_ms: wire::fixed_millis(
                    start.elapsed().as_millis(),
                    "search response duration",
                )?,
                match_count: MatchCountV1::try_from(outcome.match_count)?,
                returned_hits: 0,
                request_limit_truncated: false,
                fuzzy_work: FuzzyWorkUsageV1::try_from(outcome.fuzzy_work)?,
                hits: Vec::new(),
                diagnostics: outcome
                    .diagnostics
                    .into_iter()
                    .map(SearchDiagnosticV1::try_from)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                fallback_used: outcome.fallback_used,
            });
        }

        let mut projection_budget = CandidateProjectionBudget::new(self.policy.limits);
        let prepared = self
            .policy
            .prepare(SearchRequest::new(query.clone(), limit));
        let fetch_limit = prepared.candidate_limit();
        if fetch_limit == 0 {
            let outcome = prepared.execute(Vec::new());
            return validated_search_response(SearchResponse {
                protocol_revision: SEARCH_PROTOCOL_REVISION,
                generation: wire::generation_stamp(&self.snapshot.generation),
                query_policy_id: wire::query_policy_id(),
                query: response_query,
                took_ms: wire::fixed_millis(
                    start.elapsed().as_millis(),
                    "search response duration",
                )?,
                match_count: MatchCountV1::try_from(outcome.match_count)?,
                returned_hits: 0,
                request_limit_truncated: false,
                fuzzy_work: FuzzyWorkUsageV1::try_from(outcome.fuzzy_work)?,
                hits: Vec::new(),
                diagnostics: outcome
                    .diagnostics
                    .into_iter()
                    .map(SearchDiagnosticV1::try_from)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                fallback_used: outcome.fallback_used,
            });
        }

        let searcher = self.snapshot.reader.searcher();
        let strict_terms = prepared.retrieval_terms(RetrievalStage::Strict);
        let strict_query =
            build_search_retrieval_query(&self.snapshot.fields, prepared.query(), &strict_terms)?;
        let retrieval_limit = fetch_limit.saturating_add(1);
        let strict_docs =
            collect_stable_top_docs(&searcher, strict_query.as_ref(), retrieval_limit)?;
        let strict_evidence = EvidencePlan::new(
            &searcher,
            &self.snapshot.fields,
            prepared.query(),
            &strict_terms,
        )?;
        let query_terms = normalized_query_terms(prepared.query());
        let query_tokens = query_terms.split_whitespace().collect::<Vec<_>>();
        let no_excluded_keys = BTreeSet::new();
        let mut candidates_by_key = collect_search_candidates(
            &searcher,
            &self.snapshot.fields,
            strict_docs,
            &strict_evidence,
            &no_excluded_keys,
            &query_tokens,
            &mut projection_budget,
        )?;

        let strict_facts = candidates_by_key
            .values()
            .map(|candidate| candidate.facts.clone())
            .collect::<Vec<_>>();
        let mut fallback_candidates = BTreeMap::new();
        let mut outcome = prepared.execute_with_fallback(
            strict_facts,
            |strict_match_keys| -> Result<Vec<_>> {
                let fallback_terms = prepared.retrieval_terms(RetrievalStage::FuzzyFallback);
                let fallback_query = build_search_retrieval_query(
                    &self.snapshot.fields,
                    prepared.query(),
                    &fallback_terms,
                )?;
                let fallback_retrieval_limit =
                    retrieval_limit.saturating_add(strict_match_keys.len());
                let fallback_docs = collect_stable_top_docs(
                    &searcher,
                    fallback_query.as_ref(),
                    fallback_retrieval_limit,
                )?;
                let fallback_evidence = EvidencePlan::new(
                    &searcher,
                    &self.snapshot.fields,
                    prepared.query(),
                    &fallback_terms,
                )?;
                fallback_candidates = collect_search_candidates(
                    &searcher,
                    &self.snapshot.fields,
                    fallback_docs,
                    &fallback_evidence,
                    strict_match_keys,
                    &query_tokens,
                    &mut projection_budget,
                )?;
                Ok(fallback_candidates
                    .values()
                    .map(|candidate| candidate.facts.clone())
                    .collect())
            },
        )?;
        outcome.extend_diagnostics(std::mem::take(&mut projection_budget.diagnostics));
        candidates_by_key.extend(fallback_candidates);

        let mut hits = Vec::with_capacity(outcome.matches.len());
        let mut hits_json_bytes = 2_u64;
        let mut request_limit_truncated = outcome.request_limit_truncated;
        for ranked in outcome.matches {
            let Some(candidate) = candidates_by_key.remove(&ranked.stable_key) else {
                continue;
            };
            let hit = build_search_hit(candidate, ranked)?;
            if !push_search_hit_within_json_budget(&mut hits, hit, &mut hits_json_bytes)? {
                request_limit_truncated = true;
                break;
            }
        }
        let returned_hits = hits.len();

        validated_search_response(SearchResponse {
            protocol_revision: SEARCH_PROTOCOL_REVISION,
            generation: wire::generation_stamp(&self.snapshot.generation),
            query_policy_id: wire::query_policy_id(),
            query: response_query,
            took_ms: wire::fixed_millis(start.elapsed().as_millis(), "search response duration")?,
            match_count: MatchCountV1::try_from(outcome.match_count)?,
            returned_hits: wire::fixed_u32(returned_hits, "search response hit count")?,
            request_limit_truncated,
            fuzzy_work: FuzzyWorkUsageV1::try_from(outcome.fuzzy_work)?,
            hits,
            diagnostics: outcome
                .diagnostics
                .into_iter()
                .map(SearchDiagnosticV1::try_from)
                .collect::<std::result::Result<Vec<_>, _>>()?,
            fallback_used: outcome.fallback_used,
        })
    }

    pub(crate) fn suggest(&self, prefix: &str, limit: usize) -> Result<SuggestResponse> {
        let start = Instant::now();
        let response_prefix = prefix;
        let execution_prefix = prefix.trim();
        if execution_prefix.is_empty() || limit == 0 {
            return Ok(SuggestResponse {
                protocol_revision: SEARCH_PROTOCOL_REVISION,
                generation: wire::generation_stamp(&self.snapshot.generation),
                query_policy_id: wire::query_policy_id(),
                prefix: response_prefix.to_owned(),
                took_ms: wire::fixed_millis(
                    start.elapsed().as_millis(),
                    "suggest response duration",
                )?,
                suggestions: Vec::new(),
            });
        }

        let (want_kind, want_path, rest) = if let Some(rest) = execution_prefix.strip_prefix("t:") {
            (true, false, rest)
        } else if let Some(rest) = execution_prefix.strip_prefix("type:") {
            (true, false, rest)
        } else if let Some(rest) = execution_prefix.strip_prefix("in:") {
            (false, true, rest)
        } else {
            (true, true, execution_prefix)
        };

        let mut suggestions = Vec::new();
        if want_kind {
            let lower = rest.to_lowercase();
            for &kind in SearchKind::ALL {
                let canonical = kind.canonical_name();
                if canonical.to_lowercase().starts_with(&lower) {
                    suggestions.push(format!("t:{canonical}"));
                    if suggestions.len() >= limit {
                        break;
                    }
                }
            }
        }
        if want_path && suggestions.len() < limit {
            suggestions.extend(
                self.snapshot
                    .path_suggestions
                    .suggest(rest, limit.saturating_sub(suggestions.len())),
            );
        }
        retain_wire_suggestions(&mut suggestions);

        Ok(SuggestResponse {
            protocol_revision: SEARCH_PROTOCOL_REVISION,
            generation: wire::generation_stamp(&self.snapshot.generation),
            query_policy_id: wire::query_policy_id(),
            prefix: response_prefix.to_owned(),
            took_ms: wire::fixed_millis(start.elapsed().as_millis(), "suggest response duration")?,
            suggestions,
        })
    }
}

fn retain_wire_suggestions(suggestions: &mut Vec<String>) {
    suggestions.retain(|suggestion| SuggestResponse::validate_suggestion(suggestion).is_ok());
    while SuggestResponse::validate_suggestions(suggestions).is_err() {
        suggestions.pop();
    }
}

fn trim_owned_query(mut query: String) -> String {
    let trimmed_end = query.trim_end().len();
    query.truncate(trimmed_end);
    let trimmed_start = query.len().saturating_sub(query.trim_start().len());
    if trimmed_start != 0 {
        drop(query.drain(..trimmed_start));
    }
    query
}

fn normalized_query_terms(query: &QuerySpec) -> String {
    query
        .terms()
        .iter()
        .map(|term| to_terms(term.text()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn suggest_in_paths(sorted_directories: &[String], raw_prefix: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }

    let prefix = raw_prefix.trim();
    let start = if prefix.is_empty() {
        0
    } else {
        sorted_directories.partition_point(|path| path.as_str() < prefix)
    };
    let mut suggestions = Vec::new();
    let mut cursor = start;
    while cursor < sorted_directories.len() && suggestions.len() < limit {
        let directory = &sorted_directories[cursor];
        if !prefix.is_empty() && !directory.starts_with(prefix) {
            break;
        }
        let suggestion = immediate_path_suggestion(directory, prefix);
        suggestions.push(format!("in:{suggestion}"));

        if directory == prefix {
            cursor += 1;
        } else {
            let subtree_length = sorted_directories[cursor..]
                .partition_point(|candidate| candidate.starts_with(suggestion));
            cursor += subtree_length.max(1);
        }
    }

    suggestions
}

fn immediate_path_suggestion<'a>(directory: &'a str, prefix: &str) -> &'a str {
    let suffix = &directory[prefix.len()..];
    if suffix.is_empty() {
        return directory;
    }
    suffix.find('/').map_or(directory, |position| {
        &directory[..prefix.len() + position + 1]
    })
}

fn ranking_candidate_key(stable_id: &str, path: &str, name: &str, kind: &str) -> Result<String> {
    let components = [
        b"unity-asset-search-candidate-v1".as_slice(),
        stable_id.as_bytes(),
        path.as_bytes(),
        name.as_bytes(),
        kind.as_bytes(),
    ];
    let declared_length = components.iter().try_fold(0_u64, |total, component| {
        let component_length = DigestV1Builder::framed_len(component)
            .context("measure framed candidate identity component")?;
        total
            .checked_add(component_length)
            .ok_or_else(|| anyhow!("candidate identity length overflow"))
    })?;
    let mut digest = DigestV1Builder::new(declared_length);
    for component in components {
        digest
            .update_framed(component)
            .context("hash candidate identity component")?;
    }
    let digest = digest.finalize().context("finalize candidate identity")?;
    Ok(format!("candidate-v1:{}", hex::encode(digest.as_bytes())))
}

fn quantize_retrieval_score(score: f32) -> i64 {
    if !score.is_finite() {
        return i64::MIN;
    }
    (f64::from(score) * 1_000_000.0).round() as i64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StableTopKey<'a> {
    retrieval_score: i64,
    document_id: &'a str,
    address: DocAddress,
}

impl Ord for StableTopKey<'_> {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.retrieval_score
            .cmp(&other.retrieval_score)
            .then_with(|| other.document_id.cmp(self.document_id))
            .then_with(|| compare_doc_address(other.address, self.address))
    }
}

impl PartialOrd for StableTopKey<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StableTopHit {
    retrieval_score: i64,
    document_id: String,
    address: DocAddress,
}

impl StableTopHit {
    fn key(&self) -> StableTopKey<'_> {
        StableTopKey {
            retrieval_score: self.retrieval_score,
            document_id: &self.document_id,
            address: self.address,
        }
    }
}

impl Ord for StableTopHit {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.key().cmp(&other.key())
    }
}

impl PartialOrd for StableTopHit {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

fn compare_doc_address(left: DocAddress, right: DocAddress) -> CmpOrdering {
    left.segment_ord
        .cmp(&right.segment_ord)
        .then_with(|| left.doc_id.cmp(&right.doc_id))
}

struct StableTopDocs {
    limit: usize,
}

impl Collector for StableTopDocs {
    type Fruit = Vec<(i64, DocAddress)>;
    type Child = StableTopSegmentCollector;

    fn for_segment(
        &self,
        segment_local_id: u32,
        segment_reader: &tantivy::SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let document_ids = segment_reader
            .fast_fields()
            .str("id")?
            .ok_or_else(|| tantivy::TantivyError::SchemaError("id fast field missing".into()))?;
        Ok(StableTopSegmentCollector {
            limit: self.limit,
            segment_ord: segment_local_id,
            document_ids,
            scratch: String::new(),
            heap: BinaryHeap::with_capacity(self.limit),
            error: None,
        })
    }

    fn requires_scoring(&self) -> bool {
        true
    }

    fn merge_fruits(
        &self,
        child_fruits: Vec<<Self::Child as SegmentCollector>::Fruit>,
    ) -> tantivy::Result<Self::Fruit> {
        let mut heap = BinaryHeap::with_capacity(self.limit);
        for child_fruit in child_fruits {
            for hit in child_fruit? {
                push_stable_hit(&mut heap, hit, self.limit);
            }
        }
        let mut hits = heap.into_iter().map(|Reverse(hit)| hit).collect::<Vec<_>>();
        hits.sort_unstable_by(|left, right| right.cmp(left));
        Ok(hits
            .into_iter()
            .map(|hit| (hit.retrieval_score, hit.address))
            .collect())
    }
}

struct StableTopSegmentCollector {
    limit: usize,
    segment_ord: u32,
    document_ids: StrColumn,
    scratch: String,
    heap: BinaryHeap<Reverse<StableTopHit>>,
    error: Option<tantivy::TantivyError>,
}

impl SegmentCollector for StableTopSegmentCollector {
    type Fruit = tantivy::Result<Vec<StableTopHit>>;

    fn collect(&mut self, doc: DocId, score: Score) {
        if self.error.is_some() || self.limit == 0 {
            return;
        }
        let retrieval_score = quantize_retrieval_score(score);

        self.scratch.clear();
        let Some(term_ord) = self.document_ids.term_ords(doc).next() else {
            self.error = Some(tantivy::TantivyError::InternalError(
                "indexed document is missing its stable id".into(),
            ));
            return;
        };
        match self.document_ids.ord_to_str(term_ord, &mut self.scratch) {
            Ok(true) => {}
            Ok(false) => {
                self.error = Some(tantivy::TantivyError::InternalError(
                    "indexed document has an invalid stable id ordinal".into(),
                ));
                return;
            }
            Err(error) => {
                self.error = Some(error.into());
                return;
            }
        }

        let address = DocAddress::new(self.segment_ord, doc);
        if self.heap.len() == self.limit
            && self.heap.peek().is_some_and(|worst| {
                StableTopKey {
                    retrieval_score,
                    document_id: &self.scratch,
                    address,
                }
                .cmp(&worst.0.key())
                .is_le()
            })
        {
            return;
        }
        push_stable_hit(
            &mut self.heap,
            StableTopHit {
                retrieval_score,
                document_id: self.scratch.clone(),
                address,
            },
            self.limit,
        );
    }

    fn harvest(self) -> Self::Fruit {
        if let Some(error) = self.error {
            return Err(error);
        }
        Ok(self.heap.into_iter().map(|Reverse(hit)| hit).collect())
    }
}

fn push_stable_hit(heap: &mut BinaryHeap<Reverse<StableTopHit>>, hit: StableTopHit, limit: usize) {
    if limit == 0 {
        return;
    }
    if heap.len() < limit {
        heap.push(Reverse(hit));
        return;
    }
    if heap.peek().is_some_and(|worst| hit.cmp(&worst.0).is_gt()) {
        heap.pop();
        heap.push(Reverse(hit));
    }
}

fn collect_stable_top_docs(
    searcher: &tantivy::Searcher,
    query: &dyn Query,
    limit: usize,
) -> Result<Vec<(i64, DocAddress)>> {
    Ok(searcher.search(query, &StableTopDocs { limit })?)
}

struct EvidencePlan {
    terms: Vec<TermEvidencePlan>,
}

struct TermEvidencePlan {
    term_index: usize,
    fields: Vec<FieldEvidencePlan>,
}

struct FieldEvidencePlan {
    field: MatchField,
    exact: Box<dyn Weight>,
    prefix: Box<dyn Weight>,
    fuzzy: Option<Box<dyn Weight>>,
}

impl EvidencePlan {
    fn new(
        searcher: &tantivy::Searcher,
        fields: &SearchQueryFields,
        query: &QuerySpec,
        retrieval_terms: &[RetrievalTerm],
    ) -> Result<Self> {
        let mut grouped = BTreeMap::<usize, Vec<&RetrievalTerm>>::new();
        for term in retrieval_terms {
            grouped.entry(term.term_index).or_default().push(term);
        }

        let mut terms = Vec::new();
        for term_index in 0..query.terms().len() {
            let Some(tokens) = grouped.get(&term_index) else {
                continue;
            };
            let mut field_plans = Vec::with_capacity(1);
            for field in MatchField::ALL
                .into_iter()
                .filter(|field| field.requires_retrieval_evidence())
            {
                let index_field = retrieval_field(fields, field);
                let exact = build_exact_field_query(index_field, tokens)
                    .weight(EnableScoring::disabled_from_searcher(searcher))?;
                let prefix = build_prefix_field_query(index_field, tokens)
                    .weight(EnableScoring::disabled_from_searcher(searcher))?;
                let fuzzy = build_fuzzy_field_query(index_field, tokens)
                    .map(|query| query.weight(EnableScoring::disabled_from_searcher(searcher)))
                    .transpose()?;
                field_plans.push(FieldEvidencePlan {
                    field,
                    exact,
                    prefix,
                    fuzzy,
                });
            }
            terms.push(TermEvidencePlan {
                term_index,
                fields: field_plans,
            });
        }
        Ok(Self { terms })
    }

    fn apply(
        &self,
        searcher: &tantivy::Searcher,
        candidates: &mut [RetrievedCandidate],
    ) -> Result<()> {
        let mut ordered_addresses = candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| (candidate.address, index))
            .collect::<Vec<_>>();
        ordered_addresses
            .sort_unstable_by_key(|(address, _)| (address.segment_ord, address.doc_id));

        for term in &self.terms {
            let mut best = vec![None; candidates.len()];
            for field in &term.fields {
                let exact = weight_matches_candidates(
                    &*field.exact,
                    searcher,
                    candidates.len(),
                    &ordered_addresses,
                )?;
                let prefix = weight_matches_candidates(
                    &*field.prefix,
                    searcher,
                    candidates.len(),
                    &ordered_addresses,
                )?;
                let fuzzy = field
                    .fuzzy
                    .as_deref()
                    .map(|weight| {
                        weight_matches_candidates(
                            weight,
                            searcher,
                            candidates.len(),
                            &ordered_addresses,
                        )
                    })
                    .transpose()?;
                for candidate_index in 0..candidates.len() {
                    let kind = if exact[candidate_index] {
                        Some(MatchKind::Token)
                    } else if prefix[candidate_index] {
                        Some(if field.field == MatchField::Content {
                            MatchKind::Substring
                        } else {
                            MatchKind::Prefix
                        })
                    } else if fuzzy
                        .as_ref()
                        .is_some_and(|matches| matches[candidate_index])
                    {
                        Some(MatchKind::Fuzzy)
                    } else {
                        None
                    };
                    let Some(kind) = kind else {
                        continue;
                    };
                    let evidence = RetrievalEvidence::new(term.term_index, field.field, kind);
                    if best[candidate_index].is_none_or(|current| evidence.is_better_than(current))
                    {
                        best[candidate_index] = Some(evidence);
                    }
                }
            }
            for (candidate, evidence) in candidates.iter_mut().zip(best) {
                candidate.facts.evidence.extend(evidence);
            }
        }
        Ok(())
    }
}

fn build_exact_field_query(field: Field, terms: &[&RetrievalTerm]) -> Box<dyn Query> {
    let terms = terms
        .iter()
        .map(|term| Term::from_field_text(field, &term.text))
        .collect::<Vec<_>>();
    if let [term] = terms.as_slice() {
        Box::new(TermQuery::new(
            term.clone(),
            tantivy::schema::IndexRecordOption::Basic,
        ))
    } else {
        Box::new(PhraseQuery::new(terms))
    }
}

fn build_prefix_field_query(field: Field, terms: &[&RetrievalTerm]) -> Box<dyn Query> {
    Box::new(PhrasePrefixQuery::new(
        terms
            .iter()
            .map(|term| Term::from_field_text(field, &term.text))
            .collect(),
    ))
}

fn build_fuzzy_field_query(field: Field, terms: &[&RetrievalTerm]) -> Option<Box<dyn Query>> {
    terms
        .iter()
        .any(|term| term.fuzzy_distance.is_some())
        .then(|| {
            let queries = terms
                .iter()
                .map(|term| {
                    let term_value = Term::from_field_text(field, &term.text);
                    if let Some(distance) = term.fuzzy_distance {
                        Box::new(FuzzyTermQuery::new(term_value, distance, true)) as Box<dyn Query>
                    } else {
                        Box::new(TermQuery::new(
                            term_value,
                            tantivy::schema::IndexRecordOption::Basic,
                        )) as Box<dyn Query>
                    }
                })
                .collect();
            Box::new(BooleanQuery::intersection(queries)) as Box<dyn Query>
        })
}

fn weight_matches_candidates(
    weight: &dyn Weight,
    searcher: &tantivy::Searcher,
    candidate_count: usize,
    ordered: &[(DocAddress, usize)],
) -> Result<Vec<bool>> {
    let mut matches = vec![false; candidate_count];
    let mut cursor = 0usize;
    while cursor < ordered.len() {
        let segment_ord = ordered[cursor].0.segment_ord;
        let mut scorer = weight.scorer(searcher.segment_reader(segment_ord), 1.0)?;
        let mut current_doc = scorer.doc();
        while cursor < ordered.len() && ordered[cursor].0.segment_ord == segment_ord {
            let (address, candidate_index) = ordered[cursor];
            if current_doc < address.doc_id {
                current_doc = scorer.seek(address.doc_id);
            }
            matches[candidate_index] = current_doc == address.doc_id;
            cursor += 1;
        }
    }
    Ok(matches)
}

struct HitProjection {
    guid: Option<String>,
    stable_id: String,
    location: Location,
    matched_hierarchy_paths: Vec<String>,
    matched_script_symbols: Vec<String>,
}

struct RetrievedCandidate {
    address: DocAddress,
    facts: CandidateFacts,
    projection: HitProjection,
}

struct CandidateProjectionBudget {
    limits: SearchLimits,
    stored_document_bytes: usize,
    retained_bytes: usize,
    diagnostics: Vec<SearchDiagnostic>,
    exhausted: bool,
}

impl CandidateProjectionBudget {
    fn new(limits: SearchLimits) -> Self {
        Self {
            limits,
            stored_document_bytes: 0,
            retained_bytes: 0,
            diagnostics: Vec::new(),
            exhausted: false,
        }
    }

    fn reject(&mut self, diagnostic: SearchDiagnostic) {
        self.diagnostics.push(diagnostic);
    }

    fn try_reserve_stored_document(&mut self, document_bytes: usize) -> bool {
        if document_bytes > MAX_STORED_CANDIDATE_DOCUMENT_BYTES {
            self.reject(SearchDiagnostic::CandidateTotalByteLimitExceeded {
                consumed: document_bytes,
                limit: MAX_STORED_CANDIDATE_DOCUMENT_BYTES,
            });
            return false;
        }

        let Some(next_total) = self.stored_document_bytes.checked_add(document_bytes) else {
            self.reject(SearchDiagnostic::CandidateTotalByteLimitExceeded {
                consumed: usize::MAX,
                limit: self.limits.max_total_candidate_bytes,
            });
            self.exhausted = true;
            return false;
        };
        if next_total > self.limits.max_total_candidate_bytes {
            self.reject(SearchDiagnostic::CandidateTotalByteLimitExceeded {
                consumed: next_total,
                limit: self.limits.max_total_candidate_bytes,
            });
            self.exhausted = true;
            return false;
        }
        self.stored_document_bytes = next_total;
        true
    }

    fn try_reserve(&mut self, candidate_bytes: usize) -> bool {
        let Some(next_total) = self.retained_bytes.checked_add(candidate_bytes) else {
            self.reject(SearchDiagnostic::CandidateTotalByteLimitExceeded {
                consumed: usize::MAX,
                limit: self.limits.max_total_candidate_bytes,
            });
            self.exhausted = true;
            return false;
        };
        if next_total > self.limits.max_total_candidate_bytes {
            self.reject(SearchDiagnostic::CandidateTotalByteLimitExceeded {
                consumed: next_total,
                limit: self.limits.max_total_candidate_bytes,
            });
            self.exhausted = true;
            return false;
        }
        self.retained_bytes = next_total;
        true
    }
}

fn required_stored_text<'a>(
    document: &'a TantivyDocument,
    field: Field,
    field_name: &str,
) -> Result<&'a str> {
    document
        .get_first(field)
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("indexed document is missing required stored field `{field_name}`"))
}

fn optional_stored_text<'a>(
    document: &'a TantivyDocument,
    field: Field,
    field_name: &str,
) -> Result<Option<&'a str>> {
    document
        .get_first(field)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| anyhow!("indexed document stored field `{field_name}` is not text"))
        })
        .transpose()
}

#[derive(Debug, Clone, Copy)]
struct ValidatedProjectionFields<'a> {
    limits: SearchLimits,
    stable_id: &'a str,
    name: &'a str,
    path: &'a str,
    kind: &'a str,
    guid: Option<&'a str>,
    container_source_path: Option<&'a str>,
    bytes_without_key: usize,
}

impl<'a> ValidatedProjectionFields<'a> {
    fn new(
        limits: SearchLimits,
        stable_id: &'a str,
        name: &'a str,
        path: &'a str,
        kind: &'a str,
        guid: Option<&'a str>,
        container_source_path: Option<&'a str>,
    ) -> std::result::Result<Self, SearchDiagnostic> {
        limits.validate_field_bytes(CandidateField::StableKey, stable_id.len())?;
        let base_bytes = limits.measure_candidate("", name, path, kind, 0)?;
        if let Some(guid) = guid {
            limits.validate_field_bytes(CandidateField::Guid, guid.len())?;
        }
        if let Some(container_source_path) = container_source_path {
            limits.validate_field_bytes(
                CandidateField::ContainerSourcePath,
                container_source_path.len(),
            )?;
        }

        let bytes_without_key = base_bytes
            .checked_add(stable_id.len())
            .and_then(|size| size.checked_add(guid.map_or(0, str::len)))
            .and_then(|size| size.checked_add(container_source_path.map_or(0, str::len)))
            .ok_or(SearchDiagnostic::CandidateTotalByteLimitExceeded {
                consumed: usize::MAX,
                limit: limits.max_total_candidate_bytes,
            })?;

        Ok(Self {
            limits,
            stable_id,
            name,
            path,
            kind,
            guid,
            container_source_path,
            bytes_without_key,
        })
    }

    fn measure_with_key_and_context(
        self,
        candidate_key: &str,
        context_bytes: usize,
    ) -> std::result::Result<usize, SearchDiagnostic> {
        self.limits
            .validate_field_bytes(CandidateField::StableKey, candidate_key.len())?;
        candidate_key
            .len()
            .checked_add(self.bytes_without_key)
            .and_then(|bytes| bytes.checked_add(context_bytes))
            .ok_or(SearchDiagnostic::CandidateTotalByteLimitExceeded {
                consumed: usize::MAX,
                limit: self.limits.max_total_candidate_bytes,
            })
    }
}

#[derive(Default)]
struct StoredContext {
    hierarchy_paths: Vec<String>,
    script_symbols: Vec<String>,
    retained_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct StoredValueProjection {
    field: Field,
    field_name: &'static str,
    matched_value_limit: usize,
    stored_value_limit: usize,
    budget_field: CandidateField,
}

fn stored_value_projections(fields: &SearchQueryFields) -> [StoredValueProjection; 2] {
    [
        StoredValueProjection {
            field: fields.hierarchy_paths,
            field_name: "hierarchy_paths",
            matched_value_limit: MAX_MATCHED_HIERARCHY_PATHS,
            stored_value_limit: MAX_STORED_HIERARCHY_PATHS,
            budget_field: CandidateField::Path,
        },
        StoredValueProjection {
            field: fields.script_symbols,
            field_name: "script_symbols",
            matched_value_limit: MAX_MATCHED_SCRIPT_SYMBOLS,
            stored_value_limit: MAX_STORED_SCRIPT_SYMBOLS,
            budget_field: CandidateField::Name,
        },
    ]
}

fn validate_stored_context_counts(
    document: &TantivyDocument,
    fields: &SearchQueryFields,
    budget: &mut CandidateProjectionBudget,
) -> bool {
    let mut counts_valid = true;
    for projection in stored_value_projections(fields) {
        let actual = document.get_all(projection.field).count();
        if let Err(diagnostic) = validate_stored_value_count(
            projection.field_name,
            actual,
            projection.stored_value_limit,
        ) {
            budget.reject(diagnostic);
            counts_valid = false;
        }
    }
    counts_valid
}

fn collect_stored_context(
    document: &TantivyDocument,
    fields: &SearchQueryFields,
    query_tokens: &[&str],
    budget: &mut CandidateProjectionBudget,
) -> Result<StoredContext> {
    if query_tokens.is_empty() {
        return Ok(StoredContext::default());
    }

    let [hierarchy_projection, script_projection] = stored_value_projections(fields);
    let mut context = StoredContext::default();
    collect_matching_stored_values(
        document,
        hierarchy_projection,
        query_tokens,
        &mut context.hierarchy_paths,
        &mut context.retained_bytes,
        budget,
    )?;
    collect_matching_stored_values(
        document,
        script_projection,
        query_tokens,
        &mut context.script_symbols,
        &mut context.retained_bytes,
        budget,
    )?;
    Ok(context)
}

fn validate_stored_value_count(
    _field_name: &'static str,
    actual: usize,
    limit: usize,
) -> std::result::Result<(), SearchDiagnostic> {
    if actual > limit {
        return Err(SearchDiagnostic::CandidateEvidenceLimitExceeded { actual, limit });
    }
    Ok(())
}

fn collect_matching_stored_values(
    document: &TantivyDocument,
    projection: StoredValueProjection,
    query_tokens: &[&str],
    output: &mut Vec<String>,
    retained_bytes: &mut usize,
    budget: &mut CandidateProjectionBudget,
) -> Result<()> {
    for value in document.get_all(projection.field) {
        if output.len() >= projection.matched_value_limit {
            break;
        }
        let value = value.as_str().ok_or_else(|| {
            anyhow!(
                "indexed document stored field `{}` is not text",
                projection.field_name
            )
        })?;
        let value_terms = try_to_terms(value, |_| Ok::<(), Infallible>(())).with_context(|| {
            format!(
                "tokenize indexed document stored field `{}`",
                projection.field_name
            )
        })?;
        if !matches_any_token(&value_terms, query_tokens) {
            continue;
        }
        if let Err(diagnostic) = budget
            .limits
            .validate_field_bytes(projection.budget_field, value.len())
        {
            budget.reject(diagnostic);
            continue;
        }
        if output.last().is_some_and(|previous| previous == value) {
            continue;
        }
        let Some(next_bytes) = retained_bytes.checked_add(value.len()) else {
            budget.reject(SearchDiagnostic::CandidateTotalByteLimitExceeded {
                consumed: usize::MAX,
                limit: budget.limits.max_total_candidate_bytes,
            });
            break;
        };
        *retained_bytes = next_bytes;
        output.push(value.to_owned());
    }
    Ok(())
}

fn matches_any_token(haystack_terms: &str, tokens: &[&str]) -> bool {
    let haystack = haystack_terms.trim();
    !haystack.is_empty()
        && tokens
            .iter()
            .any(|token| !token.is_empty() && haystack.contains(token))
}

fn collect_search_candidates(
    searcher: &tantivy::Searcher,
    fields: &SearchQueryFields,
    documents: impl IntoIterator<Item = (i64, DocAddress)>,
    evidence_plan: &EvidencePlan,
    excluded_keys: &BTreeSet<String>,
    query_tokens: &[&str],
    budget: &mut CandidateProjectionBudget,
) -> Result<BTreeMap<String, RetrievedCandidate>> {
    let mut candidates_by_key = BTreeMap::<String, RetrievedCandidate>::new();
    let mut store_readers = BTreeMap::new();
    for (retrieval_score, address) in documents {
        if budget.exhausted {
            break;
        }
        let store_reader = match store_readers.entry(address.segment_ord) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let reader = searcher
                    .segment_reader(address.segment_ord)
                    .get_store_reader(1)
                    .map_err(tantivy::TantivyError::from)?;
                entry.insert(reader)
            }
        };
        let stored_document_bytes = store_reader.get_document_bytes(address.doc_id)?;
        let stored_document_length = stored_document_bytes.len();
        drop(stored_document_bytes);
        if !budget.try_reserve_stored_document(stored_document_length) {
            continue;
        }
        let retrieved: TantivyDocument = store_reader.get(address.doc_id)?;
        if !validate_stored_context_counts(&retrieved, fields, budget) {
            continue;
        }
        let stable_id = required_stored_text(&retrieved, fields.id, "id")?;
        let guid = optional_stored_text(&retrieved, fields.guid, "guid")?
            .filter(|value| !value.is_empty());
        let path = required_stored_text(&retrieved, fields.path, "path")?;
        let name = required_stored_text(&retrieved, fields.name, "name")?;
        let kind = required_stored_text(&retrieved, fields.kind, "kind")?;
        let container_source_path = optional_stored_text(
            &retrieved,
            fields.container_source_path,
            "container_source_path",
        )?
        .filter(|value| !value.is_empty());

        let projection_fields = match ValidatedProjectionFields::new(
            budget.limits,
            stable_id,
            name,
            path,
            kind,
            guid,
            container_source_path,
        ) {
            Ok(fields) => fields,
            Err(diagnostic) => {
                budget.reject(diagnostic);
                continue;
            }
        };
        let candidate_key = ranking_candidate_key(
            projection_fields.stable_id,
            projection_fields.path,
            projection_fields.name,
            projection_fields.kind,
        )?;
        if excluded_keys.contains(&candidate_key) {
            continue;
        }

        let replacing_existing = if let Some(existing) = candidates_by_key.get(&candidate_key) {
            if existing.projection.stable_id != projection_fields.stable_id
                || existing.facts.path != projection_fields.path
                || existing.facts.name != projection_fields.name
                || existing.facts.kind != projection_fields.kind
            {
                return Err(anyhow!(
                    "candidate identity digest collision for `{candidate_key}`"
                ));
            }
            if existing.facts.retrieval_score >= retrieval_score {
                continue;
            }
            true
        } else {
            false
        };

        let context = collect_stored_context(&retrieved, fields, query_tokens, budget)?;
        if !replacing_existing {
            let candidate_bytes = match projection_fields
                .measure_with_key_and_context(&candidate_key, context.retained_bytes)
            {
                Ok(candidate_bytes) => candidate_bytes,
                Err(diagnostic) => {
                    budget.reject(diagnostic);
                    continue;
                }
            };
            if !budget.try_reserve(candidate_bytes) {
                continue;
            }
        }

        let location = wire::location(
            projection_fields
                .container_source_path
                .unwrap_or(projection_fields.path)
                .to_owned(),
            projection_fields.guid.map(str::to_owned),
            None,
            None,
        )?;
        candidates_by_key.insert(
            candidate_key.clone(),
            RetrievedCandidate {
                address,
                facts: CandidateFacts::new(
                    &candidate_key,
                    projection_fields.name,
                    projection_fields.path,
                    projection_fields.kind,
                    retrieval_score,
                ),
                projection: HitProjection {
                    guid: projection_fields.guid.map(str::to_owned),
                    stable_id: projection_fields.stable_id.to_owned(),
                    location,
                    matched_hierarchy_paths: context.hierarchy_paths,
                    matched_script_symbols: context.script_symbols,
                },
            },
        );
    }

    let mut candidates = candidates_by_key.into_values().collect::<Vec<_>>();
    evidence_plan.apply(searcher, &mut candidates)?;
    Ok(candidates
        .into_iter()
        .map(|candidate| (candidate.facts.stable_key.clone(), candidate))
        .collect())
}

fn build_search_hit(
    candidate: RetrievedCandidate,
    ranked: unity_asset_search_core::RankedMatch,
) -> Result<SearchHit> {
    Ok(SearchHit {
        rank: wire::fixed_u32(ranked.rank, "search hit rank")?,
        guid: candidate.projection.guid,
        path: wire::portable_path_string(candidate.facts.path)?,
        name: candidate.facts.name,
        kind: candidate.facts.kind,
        stable_id: candidate.projection.stable_id,
        location: candidate.projection.location,
        ranking_signals: ranked.ranking_signals.into(),
        match_kind: ranked.match_kind,
        explanation: ranked.explanation.into(),
        matched_hierarchy_paths: candidate.projection.matched_hierarchy_paths,
        matched_script_symbols: candidate.projection.matched_script_symbols,
        highlight_path_ranges: ranked
            .highlight_path_ranges
            .into_iter()
            .map(HighlightRangeV1::try_from)
            .collect::<std::result::Result<Vec<_>, _>>()?,
        highlight_name_ranges: ranked
            .highlight_name_ranges
            .into_iter()
            .map(HighlightRangeV1::try_from)
            .collect::<std::result::Result<Vec<_>, _>>()?,
        highlight_path: ranked.highlight_path,
        highlight_name: ranked.highlight_name,
    })
}

fn push_search_hit_within_json_budget(
    hits: &mut Vec<SearchHit>,
    hit: SearchHit,
    encoded_bytes: &mut u64,
) -> Result<bool> {
    let hit_bytes = SearchResponse::canonical_hit_json_size(&hit)?;
    let separator_bytes = u64::from(!hits.is_empty());
    let next = encoded_bytes
        .checked_add(separator_bytes)
        .and_then(|bytes| bytes.checked_add(hit_bytes))
        .ok_or_else(|| anyhow!("search hit JSON byte count overflow"))?;
    if next > MAX_SEARCH_HITS_JSON_BYTES {
        return Ok(false);
    }
    hits.push(hit);
    *encoded_bytes = next;
    Ok(true)
}

fn validated_search_response(response: SearchResponse) -> Result<SearchResponse> {
    response.validate()?;
    Ok(response)
}

fn build_search_retrieval_query(
    fields: &SearchQueryFields,
    query: &QuerySpec,
    terms: &[RetrievalTerm],
) -> Result<Box<dyn Query>> {
    let retrieval_query = build_retrieval_query(fields, terms);
    let mut intersection = vec![retrieval_query];
    if let Some(kind) = query.type_filter() {
        let term = Term::from_field_text(fields.kind_filter, &normalize_for_match(kind));
        let term_query = TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
        intersection.push(Box::new(term_query));
    }
    if let Some(path_prefix) = query.path_prefix() {
        let pattern = format!("{}.*", regex::escape(&normalize_for_match(path_prefix)));
        intersection.push(Box::new(RegexQuery::from_pattern(
            &pattern,
            fields.path_filter,
        )?));
    }
    Ok(if intersection.len() == 1 {
        intersection.remove(0)
    } else {
        Box::new(BooleanQuery::intersection(intersection))
    })
}

fn build_retrieval_query(fields: &SearchQueryFields, terms: &[RetrievalTerm]) -> Box<dyn Query> {
    if terms.is_empty() {
        return Box::new(AllQuery);
    }

    let queries = terms
        .iter()
        .filter_map(|term| {
            let token = term.text.trim();
            (!token.is_empty()).then(|| per_token_query(fields, token, term.fuzzy_distance))
        })
        .collect::<Vec<_>>();
    if queries.is_empty() {
        Box::new(AllQuery)
    } else {
        Box::new(BooleanQuery::intersection(queries))
    }
}

fn retrieval_field(fields: &SearchQueryFields, field: MatchField) -> Field {
    match field {
        MatchField::Name => fields.name_terms,
        MatchField::Path => fields.path_terms,
        MatchField::Kind => fields.kind_terms,
        MatchField::Content => fields.content_terms,
    }
}

fn per_token_query(
    fields: &SearchQueryFields,
    token: &str,
    fuzzy_distance: Option<u8>,
) -> Box<dyn Query> {
    let queries = MatchField::ALL
        .into_iter()
        .map(|field| {
            let policy = field.retrieval_policy();
            boosted_text_queries(
                retrieval_field(fields, field),
                token,
                policy.exact_boost(),
                policy.prefix_boost(),
                policy.fuzzy_boost(),
                fuzzy_distance,
            )
        })
        .collect();
    Box::new(BooleanQuery::union(queries))
}

fn boosted_text_queries(
    field: Field,
    token: &str,
    exact_boost: f32,
    prefix_boost: f32,
    fuzzy_boost: f32,
    fuzzy_distance: Option<u8>,
) -> Box<dyn Query> {
    let mut queries = Vec::new();

    let term = Term::from_field_text(field, token);
    let exact = TermQuery::new(term.clone(), tantivy::schema::IndexRecordOption::Basic);
    let prefix = PhrasePrefixQuery::new(vec![term.clone()]);

    queries.push(Box::new(BoostQuery::new(Box::new(exact), exact_boost)) as Box<dyn Query>);
    queries.push(Box::new(BoostQuery::new(Box::new(prefix), prefix_boost)) as Box<dyn Query>);
    if let Some(distance) = fuzzy_distance {
        let fuzzy = FuzzyTermQuery::new(term, distance, true);
        queries.push(Box::new(BoostQuery::new(Box::new(fuzzy), fuzzy_boost)) as Box<dyn Query>);
    }

    Box::new(BooleanQuery::union(queries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::{AssetLoadLimits, AssetLoadUsage, BudgetError};
    use unity_asset_search_protocol::{
        RequestEnvelope, ResponseEnvelope, ResponseOperation, ResponseOutcome,
        encode_response_frame,
    };

    fn generous_asset_load_budget() -> AssetLoadBudget {
        AssetLoadBudget::new(AssetLoadLimits::default()).unwrap()
    }

    #[test]
    fn owned_query_trimming_reuses_the_request_allocation() {
        let mut query = String::with_capacity(1_024);
        query.push_str("\u{2003}  Player Controller  \u{3000}");
        let pointer = query.as_ptr();
        let capacity = query.capacity();

        let query = trim_owned_query(query);

        assert_eq!(query, "Player Controller");
        assert_eq!(query.as_ptr(), pointer);
        assert_eq!(query.capacity(), capacity);
    }

    #[test]
    fn suggestion_set_is_trimmed_to_the_wire_byte_contract() {
        let mut suggestions = (0..8).map(|_| "x".repeat(30 * 1024)).collect::<Vec<_>>();
        suggestions.insert(0, "x".repeat(32 * 1024 + 1));

        retain_wire_suggestions(&mut suggestions);

        assert!(!suggestions.is_empty());
        assert!(suggestions.len() < 8);
        SuggestResponse::validate_suggestions(&suggestions).unwrap();
    }

    #[test]
    fn escaped_search_hits_keep_the_largest_frame_safe_prefix() {
        let request_json =
            include_str!("../../../integration/search-protocol/fixtures/requests/search-v3.json")
                .replace("\"limit\":25", "\"limit\":200");
        let request: RequestEnvelope = serde_json::from_str(&request_json).unwrap();
        let fixture: ResponseEnvelope = serde_json::from_str(include_str!(
            "../../../integration/search-protocol/fixtures/responses/search-v3.json"
        ))
        .unwrap();
        let ResponseOutcome::Success(operation) = fixture.into_outcome() else {
            panic!("search fixture must be successful");
        };
        let ResponseOperation::Search(mut response) = *operation else {
            panic!("search fixture must contain a search response");
        };
        let template = response.hits[0].clone();
        let name = "&".repeat(16 * 1024);
        let highlighted_name = "&amp;".repeat(16 * 1024);
        let mut hits = Vec::new();
        let mut encoded_bytes = 2_u64;

        for ordinal in 0..200_u32 {
            let mut hit = template.clone();
            hit.rank = ordinal + 1;
            hit.name.clone_from(&name);
            hit.highlight_name = Some(highlighted_name.clone());
            if !push_search_hit_within_json_budget(&mut hits, hit, &mut encoded_bytes).unwrap() {
                break;
            }
        }

        assert!(!hits.is_empty());
        assert!(hits.len() < 200);
        response.returned_hits = u32::try_from(hits.len()).unwrap();
        response.match_count.value = 200;
        response.request_limit_truncated = true;
        response.hits = hits;
        response.validate().unwrap();

        let envelope = ResponseEnvelope::success(&request, ResponseOperation::Search(response));
        let frame = encode_response_frame(&envelope, &request).unwrap();
        assert!(frame.len() <= 16 * 1024 * 1024 + std::mem::size_of::<u32>());
    }

    #[test]
    fn path_suggestions_are_sorted_and_generation_state_only() {
        let paths = vec![
            "Assets/Zeta.prefab".to_owned(),
            "Packages/com.example/Editor/Tool.cs".to_owned(),
            "Assets/Scenes/Main.unity".to_owned(),
            "Assets/Prefabs/Hero.prefab".to_owned(),
        ];
        let mut budget = generous_asset_load_budget();
        let suggestions = PathSuggestionIndex::new(paths, &mut budget).unwrap();

        assert_eq!(
            suggestions.suggest("Assets/", 10),
            vec![
                "in:Assets/".to_owned(),
                "in:Assets/Prefabs/".to_owned(),
                "in:Assets/Scenes/".to_owned(),
            ]
        );
        assert_eq!(
            suggestions.suggest("", 10),
            vec!["in:Assets/".to_owned(), "in:Packages/".to_owned()]
        );

        let input_directories = [
            "Assets/",
            "Packages/com.example/Editor/",
            "Assets/Scenes/",
            "Assets/Prefabs/",
        ];
        let directory_bytes = input_directories
            .iter()
            .map(|directory| string_allocation_bytes(directory.len()).unwrap())
            .sum::<u64>();
        let vector_bytes = vec_allocation_bytes::<String>(input_directories.len()).unwrap();
        let unique_vector_bytes = vec_allocation_bytes::<String>(input_directories.len()).unwrap();
        let arc_bytes = arc_slice_allocation_bytes::<String>(input_directories.len()).unwrap();
        assert_eq!(
            budget.usage(),
            AssetLoadUsage {
                entries: 4,
                bytes: directory_bytes + vector_bytes + unique_vector_bytes + arc_bytes,
                members: 4,
                ..AssetLoadUsage::default()
            }
        );
    }

    #[test]
    fn dense_directories_do_not_starve_later_suggestions() {
        let mut paths = (0..2_100)
            .map(|index| format!("Assets/Dense/Early/{index:04}.prefab"))
            .collect::<Vec<_>>();
        paths.extend([
            "Assets/Dense/Later/Nested/Hero.prefab".to_owned(),
            "Assets/Sibling/Only.prefab".to_owned(),
        ]);
        let mut budget = generous_asset_load_budget();
        let suggestions = PathSuggestionIndex::new(paths, &mut budget).unwrap();

        assert_eq!(
            suggestions.suggest("Assets/", 10),
            vec![
                "in:Assets/Dense/".to_owned(),
                "in:Assets/Sibling/".to_owned(),
            ]
        );
        assert_eq!(
            suggestions.suggest("Assets/Dense/", 10),
            vec![
                "in:Assets/Dense/Early/".to_owned(),
                "in:Assets/Dense/Later/".to_owned(),
            ]
        );
    }

    #[test]
    fn final_path_suggestion_backing_is_rejected_before_allocation() {
        let directory_bytes = string_allocation_bytes("Assets/".len()).unwrap();
        let vector_bytes = vec_allocation_bytes::<String>(1).unwrap();
        let retained_bytes = directory_bytes + vector_bytes + vector_bytes;
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_bytes: retained_bytes,
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = PathSuggestionIndex::new(["Assets/Hero.prefab"], &mut budget)
            .err()
            .expect("final backing budget should reject the index");

        assert_eq!(error.to_string(), "preflight final path suggestion backing");
        assert!(matches!(
            error.downcast_ref::<BudgetError>(),
            Some(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if *limit == retained_bytes && *requested > *limit
        ));
        assert_eq!(
            budget.usage(),
            AssetLoadUsage {
                entries: 1,
                bytes: retained_bytes,
                members: 1,
                ..AssetLoadUsage::default()
            }
        );
    }

    #[test]
    fn stored_document_bytes_are_bounded_before_projection() {
        let limits = SearchLimits {
            max_total_candidate_bytes: MAX_STORED_CANDIDATE_DOCUMENT_BYTES * 2,
            ..SearchLimits::default()
        };
        let mut budget = CandidateProjectionBudget::new(limits);

        assert!(!budget.try_reserve_stored_document(MAX_STORED_CANDIDATE_DOCUMENT_BYTES + 1));
        assert_eq!(budget.stored_document_bytes, 0);
        assert!(!budget.exhausted);
        assert!(matches!(
            &budget.diagnostics[0],
            SearchDiagnostic::CandidateTotalByteLimitExceeded {
                consumed,
                limit: MAX_STORED_CANDIDATE_DOCUMENT_BYTES,
            } if *consumed == MAX_STORED_CANDIDATE_DOCUMENT_BYTES + 1
        ));
    }

    #[test]
    fn stored_document_bytes_are_bounded_across_the_query() {
        let limits = SearchLimits {
            max_total_candidate_bytes: 100,
            ..SearchLimits::default()
        };
        let mut budget = CandidateProjectionBudget::new(limits);

        assert!(budget.try_reserve_stored_document(60));
        assert!(!budget.try_reserve_stored_document(41));
        assert_eq!(budget.stored_document_bytes, 60);
        assert!(budget.exhausted);
        assert!(matches!(
            &budget.diagnostics[0],
            SearchDiagnostic::CandidateTotalByteLimitExceeded {
                consumed: 101,
                limit: 100,
            }
        ));
    }

    #[test]
    fn stored_context_value_counts_have_hard_limits() {
        for (field_name, limit) in [
            ("hierarchy_paths", MAX_STORED_HIERARCHY_PATHS),
            ("script_symbols", MAX_STORED_SCRIPT_SYMBOLS),
        ] {
            let diagnostic = validate_stored_value_count(field_name, limit + 1, limit).unwrap_err();

            assert!(matches!(
                &diagnostic,
                SearchDiagnostic::CandidateEvidenceLimitExceeded {
                    actual,
                    limit: actual_limit,
                } if *actual == limit + 1 && *actual_limit == limit
            ));
            assert!(diagnostic.may_hide_matches());
        }
    }

    #[test]
    fn stable_top_key_breaks_score_ties_by_document_id() {
        let earlier = StableTopKey {
            retrieval_score: 7,
            document_id: "a",
            address: DocAddress::new(2, 1),
        };
        let later = StableTopKey {
            retrieval_score: 7,
            document_id: "b",
            address: DocAddress::new(0, 1),
        };

        assert!(earlier > later);
    }

    #[test]
    fn non_finite_retrieval_scores_sort_last() {
        assert_eq!(quantize_retrieval_score(f32::NAN), i64::MIN);
        assert_eq!(quantize_retrieval_score(f32::INFINITY), i64::MIN);
    }
}
