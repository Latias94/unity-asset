use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

const MAX_FUZZY_FIELD_CHARS: usize = 512;
const MAX_HIGHLIGHT_FIELD_BYTES: usize = 32 * 1024;
const MAX_HIGHLIGHT_QUERY_BYTES: usize = 4 * 1024;
pub const ABSOLUTE_MAX_CANDIDATES: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueryTerm {
    text: String,
    quoted: bool,
}

impl QueryTerm {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_quoted(&self) -> bool {
        self.quoted
    }
}

impl AsRef<str> for QueryTerm {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuerySpec {
    raw: String,
    type_filter: Option<String>,
    path_prefix: Option<String>,
    terms: Vec<QueryTerm>,
    diagnostics: Vec<SearchDiagnostic>,
}

impl QuerySpec {
    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn type_filter(&self) -> Option<&str> {
        self.type_filter.as_deref()
    }

    pub fn path_prefix(&self) -> Option<&str> {
        self.path_prefix.as_deref()
    }

    pub fn terms(&self) -> &[QueryTerm] {
        &self.terms
    }

    pub fn diagnostics(&self) -> &[SearchDiagnostic] {
        &self.diagnostics
    }

    pub fn has_blocking_diagnostic(&self) -> bool {
        self.diagnostics
            .iter()
            .any(SearchDiagnostic::blocks_execution)
    }

    pub fn has_filters(&self) -> bool {
        self.type_filter.is_some() || self.path_prefix.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    Exact,
    Prefix,
    Token,
    Substring,
    Abbreviation,
    Fuzzy,
    None,
}

impl MatchKind {
    fn ranking_priority(self) -> u8 {
        match self {
            Self::Exact => 0,
            Self::Prefix => 1,
            Self::Token => 2,
            Self::Substring => 3,
            Self::Abbreviation => 4,
            Self::Fuzzy => 5,
            Self::None => 6,
        }
    }

    fn ranking_cmp(self, other: Self) -> Ordering {
        self.ranking_priority().cmp(&other.ranking_priority())
    }

    fn worse_of(self, other: Self) -> Self {
        if self.ranking_cmp(other).is_gt() {
            self
        } else {
            other
        }
    }

    fn meets(self, minimum: Self) -> bool {
        !self.ranking_cmp(minimum).is_gt()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchField {
    Name,
    Path,
    Kind,
    Content,
}

impl MatchField {
    pub const ALL: [Self; 4] = [Self::Name, Self::Path, Self::Kind, Self::Content];

    fn boost(self) -> u32 {
        match self {
            Self::Name => 4,
            Self::Path => 3,
            Self::Kind => 2,
            Self::Content => 1,
        }
    }

    pub const fn retrieval_policy(self) -> RetrievalFieldPolicy {
        match self {
            Self::Name => RetrievalFieldPolicy::new(3_000, 2_000, 500),
            Self::Path => RetrievalFieldPolicy::new(2_000, 1_500, 375),
            Self::Kind => RetrievalFieldPolicy::new(1_000, 1_000, 250),
            Self::Content => RetrievalFieldPolicy::new(1_200, 1_000, 250),
        }
    }

    pub const fn requires_retrieval_evidence(self) -> bool {
        matches!(self, Self::Content)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RetrievalFieldPolicy {
    exact_millis: u16,
    prefix_millis: u16,
    fuzzy_millis: u16,
}

impl RetrievalFieldPolicy {
    const SCALE: f32 = 1_000.0;

    const fn new(exact_millis: u16, prefix_millis: u16, fuzzy_millis: u16) -> Self {
        Self {
            exact_millis,
            prefix_millis,
            fuzzy_millis,
        }
    }

    pub fn exact_boost(self) -> f32 {
        f32::from(self.exact_millis) / Self::SCALE
    }

    pub fn prefix_boost(self) -> f32 {
        f32::from(self.prefix_millis) / Self::SCALE
    }

    pub fn fuzzy_boost(self) -> f32 {
        f32::from(self.fuzzy_millis) / Self::SCALE
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermExplanation {
    pub term: String,
    pub quoted: bool,
    pub kind: MatchKind,
    pub field: MatchField,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchExplanation {
    pub terms: Vec<TermExplanation>,
    pub fuzzy_fallback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankingSignals {
    pub field_boost: u32,
    pub fuzzy_score: i64,
    pub retrieval_stage: RetrievalStage,
    pub retrieval_score: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankedMatch {
    pub rank: usize,
    pub stable_key: String,
    pub match_kind: MatchKind,
    pub ranking_signals: RankingSignals,
    pub explanation: MatchExplanation,
    pub highlight_path_ranges: Vec<HighlightRange>,
    pub highlight_name_ranges: Vec<HighlightRange>,
    pub highlight_path: Option<String>,
    pub highlight_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalEvidence {
    pub term_index: usize,
    pub field: MatchField,
    pub kind: MatchKind,
}

impl RetrievalEvidence {
    pub const fn new(term_index: usize, field: MatchField, kind: MatchKind) -> Self {
        Self {
            term_index,
            field,
            kind,
        }
    }

    pub fn is_better_than(self, other: Self) -> bool {
        self.kind
            .ranking_cmp(other.kind)
            .then_with(|| other.field.boost().cmp(&self.field.boost()))
            .is_lt()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateFacts {
    pub stable_key: String,
    pub name: String,
    pub path: String,
    pub kind: String,
    pub retrieval_score: i64,
    pub evidence: Vec<RetrievalEvidence>,
}

impl CandidateFacts {
    pub fn new(
        stable_key: impl Into<String>,
        name: impl Into<String>,
        path: impl Into<String>,
        kind: impl Into<String>,
        retrieval_score: i64,
    ) -> Self {
        Self {
            stable_key: stable_key.into(),
            name: name.into(),
            path: path.into(),
            kind: kind.into(),
            retrieval_score,
            evidence: Vec::new(),
        }
    }

    pub fn with_evidence(mut self, evidence: impl IntoIterator<Item = RetrievalEvidence>) -> Self {
        self.evidence = evidence.into_iter().collect();
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateField {
    StableKey,
    Name,
    Path,
    Kind,
    Guid,
    ContainerSourcePath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchDiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchDiagnostic {
    EmptyQuery,
    UnterminatedQuote {
        byte_offset: usize,
    },
    EmptyQuotedTerm {
        byte_offset: usize,
    },
    MissingFilterValue {
        field: String,
    },
    DuplicateFilter {
        field: String,
    },
    UnsupportedTypeFilter {
        value: String,
    },
    CandidateLimitExceeded {
        stage: RetrievalStage,
        provided: usize,
        limit: usize,
    },
    QueryByteLimitExceeded {
        actual: usize,
        limit: usize,
    },
    QueryTermLimitExceeded {
        actual: usize,
        limit: usize,
    },
    RetrievalTermLimitExceeded {
        actual: usize,
        limit: usize,
    },
    CandidateFieldByteLimitExceeded {
        field: CandidateField,
        actual: usize,
        limit: usize,
    },
    CandidateTotalByteLimitExceeded {
        consumed: usize,
        limit: usize,
    },
    CandidateInputLimitExceeded {
        limit: usize,
    },
    CandidateEvidenceLimitExceeded {
        actual: usize,
        limit: usize,
    },
    FuzzyWorkLimitExceeded {
        attempted: usize,
        limit: usize,
    },
    InvalidRetrievalEvidence {
        term_index: usize,
    },
    DuplicateCandidateKey {
        stable_key: String,
    },
    Unknown {
        contract_version: u16,
        code: String,
        severity: SearchDiagnosticSeverity,
        blocks_execution: bool,
        details: serde_json::Value,
    },
}

impl SearchDiagnostic {
    pub const WIRE_VERSION: u16 = 1;

    pub fn code(&self) -> &str {
        match self {
            Self::EmptyQuery => "empty_query",
            Self::UnterminatedQuote { .. } => "unterminated_quote",
            Self::EmptyQuotedTerm { .. } => "empty_quoted_term",
            Self::MissingFilterValue { .. } => "missing_filter_value",
            Self::DuplicateFilter { .. } => "duplicate_filter",
            Self::UnsupportedTypeFilter { .. } => "unsupported_type_filter",
            Self::CandidateLimitExceeded { .. } => "candidate_limit_exceeded",
            Self::QueryByteLimitExceeded { .. } => "query_byte_limit_exceeded",
            Self::QueryTermLimitExceeded { .. } => "query_term_limit_exceeded",
            Self::RetrievalTermLimitExceeded { .. } => "retrieval_term_limit_exceeded",
            Self::CandidateFieldByteLimitExceeded { .. } => "candidate_field_byte_limit_exceeded",
            Self::CandidateTotalByteLimitExceeded { .. } => "candidate_total_byte_limit_exceeded",
            Self::CandidateInputLimitExceeded { .. } => "candidate_input_limit_exceeded",
            Self::CandidateEvidenceLimitExceeded { .. } => "candidate_evidence_limit_exceeded",
            Self::FuzzyWorkLimitExceeded { .. } => "fuzzy_work_limit_exceeded",
            Self::InvalidRetrievalEvidence { .. } => "invalid_retrieval_evidence",
            Self::DuplicateCandidateKey { .. } => "duplicate_candidate_key",
            Self::Unknown { code, .. } => code,
        }
    }

    pub const fn severity(&self) -> SearchDiagnosticSeverity {
        if let Self::Unknown { severity, .. } = self {
            return *severity;
        }
        if self.blocks_execution() {
            SearchDiagnosticSeverity::Error
        } else {
            SearchDiagnosticSeverity::Warning
        }
    }

    pub const fn blocks_execution(&self) -> bool {
        if let Self::Unknown {
            blocks_execution, ..
        } = self
        {
            return *blocks_execution;
        }
        !matches!(
            self,
            Self::CandidateLimitExceeded { .. }
                | Self::CandidateFieldByteLimitExceeded { .. }
                | Self::CandidateTotalByteLimitExceeded { .. }
                | Self::CandidateInputLimitExceeded { .. }
                | Self::CandidateEvidenceLimitExceeded { .. }
                | Self::FuzzyWorkLimitExceeded { .. }
                | Self::InvalidRetrievalEvidence { .. }
                | Self::DuplicateCandidateKey { .. }
        )
    }

    pub const fn may_hide_matches(&self) -> bool {
        matches!(
            self,
            Self::CandidateLimitExceeded { .. }
                | Self::CandidateFieldByteLimitExceeded { .. }
                | Self::CandidateTotalByteLimitExceeded { .. }
                | Self::CandidateInputLimitExceeded { .. }
                | Self::CandidateEvidenceLimitExceeded { .. }
                | Self::FuzzyWorkLimitExceeded { .. }
                | Self::InvalidRetrievalEvidence { .. }
                | Self::Unknown { .. }
        )
    }

    fn version(&self) -> u16 {
        match self {
            Self::Unknown {
                contract_version, ..
            } => *contract_version,
            _ => Self::WIRE_VERSION,
        }
    }

    fn details(&self) -> serde_json::Value {
        match self {
            Self::EmptyQuery => serde_json::json!({}),
            Self::UnterminatedQuote { byte_offset } | Self::EmptyQuotedTerm { byte_offset } => {
                serde_json::json!({ "byte_offset": byte_offset })
            }
            Self::MissingFilterValue { field } | Self::DuplicateFilter { field } => {
                serde_json::json!({ "field": field })
            }
            Self::UnsupportedTypeFilter { value } => serde_json::json!({ "value": value }),
            Self::CandidateLimitExceeded {
                stage,
                provided,
                limit,
            } => serde_json::json!({
                "stage": stage,
                "provided": provided,
                "limit": limit,
            }),
            Self::QueryByteLimitExceeded { actual, limit }
            | Self::QueryTermLimitExceeded { actual, limit }
            | Self::RetrievalTermLimitExceeded { actual, limit }
            | Self::CandidateEvidenceLimitExceeded { actual, limit } => {
                serde_json::json!({ "actual": actual, "limit": limit })
            }
            Self::FuzzyWorkLimitExceeded { attempted, limit } => {
                serde_json::json!({ "attempted": attempted, "limit": limit })
            }
            Self::CandidateFieldByteLimitExceeded {
                field,
                actual,
                limit,
            } => serde_json::json!({
                "field": field,
                "actual": actual,
                "limit": limit,
            }),
            Self::CandidateTotalByteLimitExceeded { consumed, limit } => {
                serde_json::json!({ "consumed": consumed, "limit": limit })
            }
            Self::CandidateInputLimitExceeded { limit } => {
                serde_json::json!({ "limit": limit })
            }
            Self::InvalidRetrievalEvidence { term_index } => {
                serde_json::json!({ "term_index": term_index })
            }
            Self::DuplicateCandidateKey { stable_key } => {
                serde_json::json!({ "stable_key": stable_key })
            }
            Self::Unknown { details, .. } => details.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SearchDiagnosticWire {
    #[serde(alias = "version")]
    contract_version: u16,
    code: String,
    severity: SearchDiagnosticSeverity,
    blocks_execution: bool,
    #[serde(default)]
    details: serde_json::Value,
}

impl Serialize for SearchDiagnostic {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SearchDiagnosticWire {
            contract_version: self.version(),
            code: self.code().to_string(),
            severity: self.severity(),
            blocks_execution: self.blocks_execution(),
            details: self.details(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SearchDiagnostic {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SearchDiagnosticWire::deserialize(deserializer)?;
        if wire.contract_version != Self::WIRE_VERSION {
            return Ok(Self::Unknown {
                contract_version: wire.contract_version,
                code: wire.code,
                severity: wire.severity,
                blocks_execution: wire.blocks_execution,
                details: wire.details,
            });
        }

        let diagnostic = match wire.code.as_str() {
            "empty_query" => Self::EmptyQuery,
            "unterminated_quote" => Self::UnterminatedQuote {
                byte_offset: diagnostic_detail(&wire.details, "byte_offset")?,
            },
            "empty_quoted_term" => Self::EmptyQuotedTerm {
                byte_offset: diagnostic_detail(&wire.details, "byte_offset")?,
            },
            "missing_filter_value" => Self::MissingFilterValue {
                field: diagnostic_detail(&wire.details, "field")?,
            },
            "duplicate_filter" => Self::DuplicateFilter {
                field: diagnostic_detail(&wire.details, "field")?,
            },
            "unsupported_type_filter" => Self::UnsupportedTypeFilter {
                value: diagnostic_detail(&wire.details, "value")?,
            },
            "candidate_limit_exceeded" => Self::CandidateLimitExceeded {
                stage: diagnostic_detail(&wire.details, "stage")?,
                provided: diagnostic_detail(&wire.details, "provided")?,
                limit: diagnostic_detail(&wire.details, "limit")?,
            },
            "query_byte_limit_exceeded" => Self::QueryByteLimitExceeded {
                actual: diagnostic_detail(&wire.details, "actual")?,
                limit: diagnostic_detail(&wire.details, "limit")?,
            },
            "query_term_limit_exceeded" => Self::QueryTermLimitExceeded {
                actual: diagnostic_detail(&wire.details, "actual")?,
                limit: diagnostic_detail(&wire.details, "limit")?,
            },
            "retrieval_term_limit_exceeded" => Self::RetrievalTermLimitExceeded {
                actual: diagnostic_detail(&wire.details, "actual")?,
                limit: diagnostic_detail(&wire.details, "limit")?,
            },
            "candidate_field_byte_limit_exceeded" => Self::CandidateFieldByteLimitExceeded {
                field: diagnostic_detail(&wire.details, "field")?,
                actual: diagnostic_detail(&wire.details, "actual")?,
                limit: diagnostic_detail(&wire.details, "limit")?,
            },
            "candidate_total_byte_limit_exceeded" => Self::CandidateTotalByteLimitExceeded {
                consumed: diagnostic_detail(&wire.details, "consumed")?,
                limit: diagnostic_detail(&wire.details, "limit")?,
            },
            "candidate_input_limit_exceeded" => Self::CandidateInputLimitExceeded {
                limit: diagnostic_detail(&wire.details, "limit")?,
            },
            "candidate_evidence_limit_exceeded" => Self::CandidateEvidenceLimitExceeded {
                actual: diagnostic_detail(&wire.details, "actual")?,
                limit: diagnostic_detail(&wire.details, "limit")?,
            },
            "fuzzy_work_limit_exceeded" => Self::FuzzyWorkLimitExceeded {
                attempted: diagnostic_detail(&wire.details, "attempted")?,
                limit: diagnostic_detail(&wire.details, "limit")?,
            },
            "invalid_retrieval_evidence" => Self::InvalidRetrievalEvidence {
                term_index: diagnostic_detail(&wire.details, "term_index")?,
            },
            "duplicate_candidate_key" => Self::DuplicateCandidateKey {
                stable_key: diagnostic_detail(&wire.details, "stable_key")?,
            },
            _ => {
                return Ok(Self::Unknown {
                    contract_version: wire.contract_version,
                    code: wire.code,
                    severity: wire.severity,
                    blocks_execution: wire.blocks_execution,
                    details: wire.details,
                });
            }
        };

        if diagnostic.severity() != wire.severity
            || diagnostic.blocks_execution() != wire.blocks_execution
        {
            return Err(D::Error::custom(format!(
                "diagnostic `{}` has inconsistent severity or blocking semantics",
                diagnostic.code()
            )));
        }
        Ok(diagnostic)
    }
}

fn diagnostic_detail<T, E>(details: &serde_json::Value, field: &str) -> Result<T, E>
where
    T: DeserializeOwned,
    E: serde::de::Error,
{
    let value = details
        .get(field)
        .cloned()
        .ok_or_else(|| E::custom(format!("diagnostic details are missing `{field}`")))?;
    serde_json::from_value(value).map_err(E::custom)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalTerm {
    pub term_index: usize,
    pub text: String,
    pub fuzzy_distance: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalStage {
    Strict,
    FuzzyFallback,
}

impl RetrievalStage {
    fn ranking_priority(self) -> u8 {
        match self {
            Self::Strict => 0,
            Self::FuzzyFallback => 1,
        }
    }
}

impl SearchRequest {
    pub fn new(query: impl Into<String>, limit: usize) -> Self {
        Self {
            query: query.into(),
            limit,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FuzzyFallbackPolicy {
    pub minimum_confident_matches: usize,
    pub minimum_confident_kind: MatchKind,
    pub minimum_query_chars: usize,
    pub maximum_query_chars: usize,
    pub maximum_edit_distance: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchLimits {
    pub max_query_bytes: usize,
    pub max_query_terms: usize,
    pub max_retrieval_terms: usize,
    pub max_candidate_inputs: usize,
    pub max_stable_key_bytes: usize,
    pub max_name_bytes: usize,
    pub max_path_bytes: usize,
    pub max_kind_bytes: usize,
    pub max_guid_bytes: usize,
    pub max_container_source_path_bytes: usize,
    pub max_evidence_items: usize,
    pub max_total_candidate_bytes: usize,
    pub max_fuzzy_work_units: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            max_query_bytes: 4 * 1024,
            max_query_terms: 128,
            max_retrieval_terms: 256,
            max_candidate_inputs: ABSOLUTE_MAX_CANDIDATES,
            max_stable_key_bytes: 4 * 1024,
            max_name_bytes: 16 * 1024,
            max_path_bytes: 32 * 1024,
            max_kind_bytes: 1024,
            max_guid_bytes: 128,
            max_container_source_path_bytes: 32 * 1024,
            max_evidence_items: 256,
            max_total_candidate_bytes: 4 * 1024 * 1024,
            max_fuzzy_work_units: 2_000_000,
        }
    }
}

impl SearchLimits {
    pub fn validate_field_bytes(
        self,
        field: CandidateField,
        actual: usize,
    ) -> Result<(), SearchDiagnostic> {
        let limit = match field {
            CandidateField::StableKey => self.max_stable_key_bytes,
            CandidateField::Name => self.max_name_bytes,
            CandidateField::Path => self.max_path_bytes,
            CandidateField::Kind => self.max_kind_bytes,
            CandidateField::Guid => self.max_guid_bytes,
            CandidateField::ContainerSourcePath => self.max_container_source_path_bytes,
        };
        if actual > limit {
            return Err(SearchDiagnostic::CandidateFieldByteLimitExceeded {
                field,
                actual,
                limit,
            });
        }
        Ok(())
    }

    pub fn measure_candidate(
        self,
        stable_key: &str,
        name: &str,
        path: &str,
        kind: &str,
        evidence_items: usize,
    ) -> Result<usize, SearchDiagnostic> {
        for (field, actual) in [
            (CandidateField::StableKey, stable_key.len()),
            (CandidateField::Name, name.len()),
            (CandidateField::Path, path.len()),
            (CandidateField::Kind, kind.len()),
        ] {
            self.validate_field_bytes(field, actual)?;
        }
        if evidence_items > self.max_evidence_items {
            return Err(SearchDiagnostic::CandidateEvidenceLimitExceeded {
                actual: evidence_items,
                limit: self.max_evidence_items,
            });
        }

        stable_key
            .len()
            .checked_add(name.len())
            .and_then(|size| size.checked_add(path.len()))
            .and_then(|size| size.checked_add(kind.len()))
            .and_then(|size| {
                size.checked_add(
                    evidence_items.saturating_mul(std::mem::size_of::<RetrievalEvidence>()),
                )
            })
            .ok_or(SearchDiagnostic::CandidateTotalByteLimitExceeded {
                consumed: usize::MAX,
                limit: self.max_total_candidate_bytes,
            })
    }
}

impl Default for FuzzyFallbackPolicy {
    fn default() -> Self {
        Self {
            minimum_confident_matches: 1,
            minimum_confident_kind: MatchKind::Abbreviation,
            minimum_query_chars: 3,
            maximum_query_chars: 64,
            maximum_edit_distance: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPolicy {
    pub max_candidates: usize,
    pub candidate_multiplier: usize,
    pub filtered_candidate_multiplier: usize,
    pub fuzzy_fallback: FuzzyFallbackPolicy,
    pub limits: SearchLimits,
}

impl Default for SearchPolicy {
    fn default() -> Self {
        Self {
            max_candidates: 200,
            candidate_multiplier: 5,
            filtered_candidate_multiplier: 30,
            fuzzy_fallback: FuzzyFallbackPolicy::default(),
            limits: SearchLimits::default(),
        }
    }
}

impl SearchPolicy {
    pub fn prepare(self, request: SearchRequest) -> PreparedSearch {
        let mut query = if request.query.len() > self.limits.max_query_bytes {
            QuerySpec {
                raw: String::new(),
                type_filter: None,
                path_prefix: None,
                terms: Vec::new(),
                diagnostics: vec![SearchDiagnostic::QueryByteLimitExceeded {
                    actual: request.query.len(),
                    limit: self.limits.max_query_bytes,
                }],
            }
        } else {
            parse_query(&request.query)
        };
        if query.terms.len() > self.limits.max_query_terms {
            query
                .diagnostics
                .push(SearchDiagnostic::QueryTermLimitExceeded {
                    actual: query.terms.len(),
                    limit: self.limits.max_query_terms,
                });
        }
        let retrieval_term_count = query.terms.iter().fold(0usize, |count, term| {
            count.saturating_add(to_terms(&term.text).split_whitespace().count())
        });
        if retrieval_term_count > self.limits.max_retrieval_terms {
            query
                .diagnostics
                .push(SearchDiagnostic::RetrievalTermLimitExceeded {
                    actual: retrieval_term_count,
                    limit: self.limits.max_retrieval_terms,
                });
        }
        PreparedSearch {
            policy: self,
            request,
            query,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreparedSearch {
    policy: SearchPolicy,
    request: SearchRequest,
    query: QuerySpec,
}

impl PreparedSearch {
    pub fn query(&self) -> &QuerySpec {
        &self.query
    }

    pub fn candidate_limit(&self) -> usize {
        if self.request.limit == 0 || self.query.has_blocking_diagnostic() {
            return 0;
        }

        let multiplier = if self.query.has_filters() {
            self.policy.filtered_candidate_multiplier
        } else {
            self.policy.candidate_multiplier
        };
        self.request
            .limit
            .saturating_mul(multiplier)
            .min(self.policy.max_candidates)
            .min(ABSOLUTE_MAX_CANDIDATES)
    }

    pub fn retrieval_terms(&self, stage: RetrievalStage) -> Vec<RetrievalTerm> {
        if self.query.has_blocking_diagnostic() {
            return Vec::new();
        }
        let mut retrieval_terms = Vec::new();
        for (term_index, query_term) in self.query.terms.iter().enumerate() {
            let normalized = to_terms(&query_term.text);
            for term in normalized.split_whitespace() {
                let char_count = term.chars().count();
                let fuzzy_distance = (stage == RetrievalStage::FuzzyFallback)
                    .then(|| {
                        fuzzy_retrieval_distance(
                            query_term.quoted,
                            char_count,
                            self.policy.fuzzy_fallback,
                        )
                    })
                    .flatten();
                retrieval_terms.push(RetrievalTerm {
                    term_index,
                    text: term.to_string(),
                    fuzzy_distance,
                });
            }
        }
        retrieval_terms
    }

    pub fn execute(&self, candidates: impl IntoIterator<Item = CandidateFacts>) -> SearchOutcome {
        let mut diagnostics = self.query.diagnostics.clone();
        if let Some(outcome) = self.empty_outcome(&diagnostics) {
            return outcome;
        }

        let mut selection = CandidateSelection::new(self.candidate_limit());
        selection.ingest(
            candidates,
            RetrievalStage::Strict,
            &self.query,
            self.policy.fuzzy_fallback,
            self.policy.limits,
            &mut diagnostics,
        );
        let strict = self.evaluate_strict(&selection);
        self.finish_selection(selection, diagnostics, strict.fallback_used)
    }

    pub fn execute_with_fallback<S, F, O, E>(
        &self,
        strict_candidates: S,
        retrieve_fallback: F,
    ) -> Result<SearchOutcome, E>
    where
        S: IntoIterator<Item = CandidateFacts>,
        F: FnOnce(&BTreeSet<String>) -> Result<O, E>,
        O: IntoIterator<Item = CandidateFacts>,
    {
        let mut diagnostics = self.query.diagnostics.clone();
        if let Some(outcome) = self.empty_outcome(&diagnostics) {
            return Ok(outcome);
        }

        let mut selection = CandidateSelection::new(self.candidate_limit());
        selection.ingest(
            strict_candidates,
            RetrievalStage::Strict,
            &self.query,
            self.policy.fuzzy_fallback,
            self.policy.limits,
            &mut diagnostics,
        );
        let strict = self.evaluate_strict(&selection);
        if strict.fallback_used {
            selection.ingest(
                retrieve_fallback(&strict.matching_keys)?,
                RetrievalStage::FuzzyFallback,
                &self.query,
                self.policy.fuzzy_fallback,
                self.policy.limits,
                &mut diagnostics,
            );
        }
        Ok(self.finish_selection(selection, diagnostics, strict.fallback_used))
    }

    fn empty_outcome(&self, diagnostics: &[SearchDiagnostic]) -> Option<SearchOutcome> {
        (self.query.has_blocking_diagnostic() || self.request.limit == 0).then(|| SearchOutcome {
            query: self.query.clone(),
            matches: Vec::new(),
            diagnostics: diagnostics.to_vec(),
            fallback_used: false,
            match_count: MatchCount {
                value: 0,
                relation: if self.request.limit == 0 {
                    MatchCountRelation::LowerBound
                } else {
                    MatchCountRelation::Exact
                },
            },
            request_limit_truncated: false,
            candidates_provided: 0,
            candidates_eligible: 0,
            candidates_considered: 0,
            fuzzy_work: FuzzyWorkUsage::new(self.policy.limits.max_fuzzy_work_units),
        })
    }

    fn evaluate_strict(&self, selection: &CandidateSelection) -> StrictEvaluation {
        let terms: Vec<_> = self
            .query
            .terms
            .iter()
            .map(PreparedQueryTerm::new)
            .collect();
        let mut matching_keys = BTreeSet::new();
        let mut strict_kinds = Vec::new();
        for candidate in selection.bounded_strict() {
            if let Some(kind) = rank_candidate_kind(&terms, candidate, &self.policy.fuzzy_fallback)
            {
                matching_keys.insert(candidate.stable_key.clone());
                strict_kinds.push(kind);
            }
        }
        StrictEvaluation {
            fallback_used: should_use_fuzzy_fallback(
                &self.query,
                strict_kinds,
                self.policy.fuzzy_fallback,
            ),
            matching_keys,
        }
    }

    fn finish_selection(
        &self,
        selection: CandidateSelection,
        mut diagnostics: Vec<SearchDiagnostic>,
        fallback_used: bool,
    ) -> SearchOutcome {
        let selection = selection.finish(&mut diagnostics);
        let active_candidates = selection
            .strict
            .iter()
            .map(|candidate| (RetrievalStage::Strict, candidate))
            .chain(
                fallback_used
                    .then_some(selection.fallback.iter())
                    .into_iter()
                    .flatten()
                    .map(|candidate| (RetrievalStage::FuzzyFallback, candidate)),
            );
        let (mut ranked, fuzzy_work, fuzzy_diagnostic) = rank_candidates(
            &self.query,
            active_candidates,
            &self.policy.fuzzy_fallback,
            fallback_used,
            self.policy.limits.max_fuzzy_work_units,
        );
        if let Some(diagnostic) = fuzzy_diagnostic {
            diagnostics.push(diagnostic);
        }

        ranked.sort_by(compare_ranked);
        let mut seen = BTreeSet::new();
        ranked.retain(|ranked| seen.insert(ranked.match_.stable_key.clone()));
        let match_count = MatchCount {
            value: ranked.len(),
            relation: if diagnostics.iter().any(SearchDiagnostic::may_hide_matches) {
                MatchCountRelation::LowerBound
            } else {
                MatchCountRelation::Exact
            },
        };
        let request_limit_truncated = match_count.value > self.request.limit;
        ranked.truncate(self.request.limit);
        for (index, ranked) in ranked.iter_mut().enumerate() {
            ranked.match_.rank = index + 1;
            ranked.add_highlights(&self.query.terms);
        }

        SearchOutcome {
            query: self.query.clone(),
            matches: ranked.into_iter().map(|ranked| ranked.match_).collect(),
            diagnostics,
            fallback_used,
            match_count,
            request_limit_truncated,
            candidates_provided: selection.provided,
            candidates_eligible: selection.eligible,
            candidates_considered: selection.strict.len()
                + usize::from(fallback_used) * selection.fallback.len(),
            fuzzy_work,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchCountRelation {
    Exact,
    LowerBound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchCount {
    pub value: usize,
    pub relation: MatchCountRelation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FuzzyWorkUsage {
    pub consumed: usize,
    pub limit: usize,
    pub exhausted: bool,
}

impl FuzzyWorkUsage {
    const fn new(limit: usize) -> Self {
        Self {
            consumed: 0,
            limit,
            exhausted: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchOutcome {
    pub query: QuerySpec,
    pub matches: Vec<RankedMatch>,
    pub diagnostics: Vec<SearchDiagnostic>,
    pub fallback_used: bool,
    pub match_count: MatchCount,
    pub request_limit_truncated: bool,
    pub candidates_provided: usize,
    pub candidates_eligible: usize,
    pub candidates_considered: usize,
    pub fuzzy_work: FuzzyWorkUsage,
}

impl SearchOutcome {
    pub fn extend_diagnostics(&mut self, diagnostics: impl IntoIterator<Item = SearchDiagnostic>) {
        let mut may_hide_matches = false;
        self.diagnostics
            .extend(diagnostics.into_iter().inspect(|diagnostic| {
                may_hide_matches |= diagnostic.may_hide_matches();
            }));
        if may_hide_matches {
            self.match_count.relation = MatchCountRelation::LowerBound;
        }
    }
}

pub fn normalize_for_match(input: &str) -> String {
    input.nfkc().collect::<String>().to_lowercase()
}

pub fn parse_query(input: &str) -> QuerySpec {
    let (lexemes, mut diagnostics) = lex_query(input);
    let mut type_filter = None;
    let mut path_prefix = None;
    let mut terms = Vec::new();

    for lexeme in lexemes {
        let field = (!lexeme.whole_quoted)
            .then(|| lexeme.text.split_once(':'))
            .flatten()
            .map(|(field, value)| (field.to_ascii_lowercase(), value));
        match field {
            Some((field, value)) if field == "t" || field == "type" => {
                if value.trim().is_empty() {
                    diagnostics.push(SearchDiagnostic::MissingFilterValue {
                        field: "type".to_string(),
                    });
                } else if type_filter.is_some() {
                    diagnostics.push(SearchDiagnostic::DuplicateFilter {
                        field: "type".to_string(),
                    });
                } else if let Some(canonical) = canonicalize_type_filter(value) {
                    type_filter = Some(canonical);
                } else {
                    diagnostics.push(SearchDiagnostic::UnsupportedTypeFilter {
                        value: value.trim().to_string(),
                    });
                }
            }
            Some((field, value)) if field == "in" => {
                if value.trim().is_empty() {
                    diagnostics.push(SearchDiagnostic::MissingFilterValue {
                        field: "in".to_string(),
                    });
                } else if path_prefix.is_some() {
                    diagnostics.push(SearchDiagnostic::DuplicateFilter {
                        field: "in".to_string(),
                    });
                } else {
                    path_prefix = Some(value.trim().to_string());
                }
            }
            _ => terms.push(QueryTerm {
                text: lexeme.text,
                quoted: lexeme.whole_quoted,
            }),
        }
    }

    if input.trim().is_empty() {
        diagnostics.push(SearchDiagnostic::EmptyQuery);
    }

    QuerySpec {
        raw: input.to_string(),
        type_filter,
        path_prefix,
        terms,
        diagnostics,
    }
}

#[derive(Debug)]
struct Lexeme {
    text: String,
    whole_quoted: bool,
}

fn lex_query(input: &str) -> (Vec<Lexeme>, Vec<SearchDiagnostic>) {
    let mut lexemes = Vec::new();
    let mut diagnostics = Vec::new();
    let mut chars = input.char_indices().peekable();

    while let Some((_, ch)) = chars.peek().copied() {
        if !ch.is_whitespace() {
            break;
        }
        chars.next();
    }

    while chars.peek().is_some() {
        let mut text = String::new();
        let mut saw_quote = false;
        let mut saw_unquoted_text = false;
        let mut quote_start = None;

        while let Some((offset, ch)) = chars.peek().copied() {
            if ch.is_whitespace() && quote_start.is_none() {
                break;
            }
            chars.next();
            if ch == '"' {
                saw_quote = true;
                if let Some(start) = quote_start.take() {
                    if text.is_empty() {
                        diagnostics.push(SearchDiagnostic::EmptyQuotedTerm { byte_offset: start });
                    }
                } else {
                    quote_start = Some(offset);
                }
                continue;
            }
            saw_unquoted_text |= quote_start.is_none();
            text.push(ch);
        }

        if let Some(byte_offset) = quote_start {
            diagnostics.push(SearchDiagnostic::UnterminatedQuote { byte_offset });
        }
        if !text.is_empty() || saw_quote {
            lexemes.push(Lexeme {
                text,
                whole_quoted: saw_quote && !saw_unquoted_text,
            });
        }

        while let Some((_, ch)) = chars.peek().copied() {
            if !ch.is_whitespace() {
                break;
            }
            chars.next();
        }
    }

    (lexemes, diagnostics)
}

fn canonicalize_type_filter(raw: &str) -> Option<String> {
    let normalized = normalize_for_match(raw.trim());
    let canonical = match normalized.as_str() {
        "prefab" => "Prefab",
        "scene" => "Scene",
        "material" | "mat" => "Material",
        "script" | "cs" => "Script",
        "animation" | "animationclip" | "anim" => "AnimationClip",
        "animator" | "animatorcontroller" | "controller" => "AnimatorController",
        "asset" => "Asset",
        "shader" => "Shader",
        "texture" | "tex" => "Texture",
        "audio" => "Audio",
        "bundlecontainer" | "container" | "bundle-container" => "BundleContainer",
        "file" => "File",
        _ => return None,
    };
    Some(canonical.to_string())
}

pub fn to_terms(input: &str) -> String {
    let normalized: Vec<char> = input.nfkc().collect();
    let mut out = String::with_capacity(input.len());
    let mut previous: Option<char> = None;

    for (index, ch) in normalized.iter().copied().enumerate() {
        if is_term_separator(ch) {
            push_term_boundary(&mut out);
            previous = None;
            continue;
        }

        let next = normalized.get(index + 1).copied();
        if let Some(previous) = previous {
            let camel_boundary = ch.is_uppercase()
                && (previous.is_lowercase()
                    || (previous.is_uppercase() && next.is_some_and(|next| next.is_lowercase())));
            let digit_boundary = ch.is_numeric() != previous.is_numeric();
            if camel_boundary || digit_boundary {
                push_term_boundary(&mut out);
            }
        }

        for lower in ch.to_lowercase() {
            out.push(lower);
        }
        previous = Some(ch);
    }

    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_term_separator(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '/' | '\\'
                | '.'
                | '-'
                | '_'
                | ':'
                | ';'
                | ','
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '"'
                | '\''
        )
}

fn push_term_boundary(out: &mut String) {
    if !out.is_empty() && !out.ends_with(' ') {
        out.push(' ');
    }
}

fn compare_candidate_priority(left: &CandidateFacts, right: &CandidateFacts) -> Ordering {
    right
        .retrieval_score
        .cmp(&left.retrieval_score)
        .then_with(|| left.stable_key.cmp(&right.stable_key))
        .then_with(|| left.path.cmp(&right.path))
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.kind.cmp(&right.kind))
}

fn merge_duplicate_candidate(existing: &mut CandidateFacts, mut incoming: CandidateFacts) {
    let incoming_is_preferred = compare_candidate_priority(&incoming, existing).is_lt();
    let mut evidence = std::mem::take(&mut existing.evidence);
    evidence.append(&mut incoming.evidence);
    evidence.sort_by(|left, right| {
        left.term_index
            .cmp(&right.term_index)
            .then_with(|| left.kind.ranking_cmp(right.kind))
            .then_with(|| right.field.boost().cmp(&left.field.boost()))
    });
    evidence.dedup();

    if incoming_is_preferred {
        incoming.evidence = evidence;
        *existing = incoming;
    } else {
        existing.evidence = evidence;
    }
}

#[derive(Debug)]
struct StrictEvaluation {
    fallback_used: bool,
    matching_keys: BTreeSet<String>,
}

#[derive(Debug)]
struct CandidateSelection {
    limit: usize,
    strict: BTreeMap<String, CandidateFacts>,
    fallback: BTreeMap<String, CandidateFacts>,
    duplicate_keys: BTreeSet<String>,
    provided: usize,
    eligible: usize,
    total_bytes: usize,
    input_limit_reported: bool,
    byte_budget_exhausted: bool,
}

impl CandidateSelection {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            strict: BTreeMap::new(),
            fallback: BTreeMap::new(),
            duplicate_keys: BTreeSet::new(),
            provided: 0,
            eligible: 0,
            total_bytes: 0,
            input_limit_reported: false,
            byte_budget_exhausted: false,
        }
    }

    fn bounded_strict(&self) -> Vec<&CandidateFacts> {
        let mut candidates: Vec<_> = self.strict.values().collect();
        candidates.sort_by(|left, right| compare_candidate_priority(left, right));
        candidates.truncate(self.limit);
        candidates
    }

    fn ingest(
        &mut self,
        candidates: impl IntoIterator<Item = CandidateFacts>,
        stage: RetrievalStage,
        query: &QuerySpec,
        fuzzy_policy: FuzzyFallbackPolicy,
        limits: SearchLimits,
        diagnostics: &mut Vec<SearchDiagnostic>,
    ) {
        if self.byte_budget_exhausted {
            return;
        }
        let fuzzy_evidence_terms: Vec<_> = query
            .terms
            .iter()
            .map(|term| query_term_supports_fuzzy_retrieval(term, fuzzy_policy))
            .collect();
        let mut candidates = candidates.into_iter();
        while self.provided < limits.max_candidate_inputs {
            let Some(candidate) = candidates.next() else {
                break;
            };
            self.provided = self.provided.saturating_add(1);
            let candidate_bytes = match candidate_size(&candidate, limits) {
                Ok(size) => size,
                Err(diagnostic) => {
                    diagnostics.push(diagnostic);
                    continue;
                }
            };
            let Some(next_total) = self.total_bytes.checked_add(candidate_bytes) else {
                diagnostics.push(SearchDiagnostic::CandidateTotalByteLimitExceeded {
                    consumed: usize::MAX,
                    limit: limits.max_total_candidate_bytes,
                });
                self.byte_budget_exhausted = true;
                break;
            };
            if next_total > limits.max_total_candidate_bytes {
                diagnostics.push(SearchDiagnostic::CandidateTotalByteLimitExceeded {
                    consumed: next_total,
                    limit: limits.max_total_candidate_bytes,
                });
                self.byte_budget_exhausted = true;
                break;
            }
            self.total_bytes = next_total;

            if let Some(invalid) = candidate.evidence.iter().find(|evidence| {
                !retrieval_evidence_is_valid(evidence, query.terms.len(), &fuzzy_evidence_terms)
            }) {
                diagnostics.push(SearchDiagnostic::InvalidRetrievalEvidence {
                    term_index: invalid.term_index,
                });
                continue;
            }
            if !candidate_matches_filters(query, &candidate) {
                continue;
            }

            let selected = match stage {
                RetrievalStage::Strict => &mut self.strict,
                RetrievalStage::FuzzyFallback => &mut self.fallback,
            };
            if let Some(existing) = selected.get_mut(&candidate.stable_key) {
                self.duplicate_keys.insert(candidate.stable_key.clone());
                merge_duplicate_candidate(existing, candidate);
            } else {
                self.eligible = self.eligible.saturating_add(1);
                selected.insert(candidate.stable_key.clone(), candidate);
            }
        }

        if !self.input_limit_reported
            && self.provided == limits.max_candidate_inputs
            && candidates.next().is_some()
        {
            diagnostics.push(SearchDiagnostic::CandidateInputLimitExceeded {
                limit: limits.max_candidate_inputs,
            });
            self.input_limit_reported = true;
        }
    }

    fn finish(mut self, diagnostics: &mut Vec<SearchDiagnostic>) -> BoundedCandidates {
        for stable_key in std::mem::take(&mut self.duplicate_keys) {
            diagnostics.push(SearchDiagnostic::DuplicateCandidateKey { stable_key });
        }
        for (stage, stage_eligible) in [
            (RetrievalStage::Strict, self.strict.len()),
            (RetrievalStage::FuzzyFallback, self.fallback.len()),
        ] {
            if stage_eligible > self.limit {
                diagnostics.push(SearchDiagnostic::CandidateLimitExceeded {
                    stage,
                    provided: stage_eligible,
                    limit: self.limit,
                });
            }
        }

        BoundedCandidates {
            strict: bound_candidates(self.strict, self.limit),
            fallback: bound_candidates(self.fallback, self.limit),
            provided: self.provided,
            eligible: self.eligible,
        }
    }
}

fn bound_candidates(
    candidates: BTreeMap<String, CandidateFacts>,
    limit: usize,
) -> Vec<CandidateFacts> {
    let mut candidates: Vec<_> = candidates.into_values().collect();
    candidates.sort_by(compare_candidate_priority);
    candidates.truncate(limit);
    candidates
}

#[derive(Debug)]
struct BoundedCandidates {
    strict: Vec<CandidateFacts>,
    fallback: Vec<CandidateFacts>,
    provided: usize,
    eligible: usize,
}

fn candidate_size(
    candidate: &CandidateFacts,
    limits: SearchLimits,
) -> Result<usize, SearchDiagnostic> {
    limits.measure_candidate(
        &candidate.stable_key,
        &candidate.name,
        &candidate.path,
        &candidate.kind,
        candidate.evidence.len(),
    )
}

#[derive(Debug)]
struct PreparedQueryTerm<'a> {
    source: &'a QueryTerm,
    normalized: String,
    normalized_chars: Vec<char>,
    tokenized: String,
}

impl<'a> PreparedQueryTerm<'a> {
    fn new(source: &'a QueryTerm) -> Self {
        let normalized = normalize_for_match(source.text.trim());
        Self {
            source,
            normalized_chars: normalized.chars().collect(),
            normalized,
            tokenized: to_terms(&source.text),
        }
    }
}

#[derive(Debug)]
struct PreparedField<'a> {
    source: &'a str,
    normalized: String,
    tokenized: String,
    fuzzy_normalized: String,
    fuzzy_char_count: usize,
    fuzzy_tokens: Vec<Vec<char>>,
}

impl<'a> PreparedField<'a> {
    fn new(source: &'a str) -> Self {
        let fuzzy_source = prefix_by_chars(source, MAX_FUZZY_FIELD_CHARS);
        let fuzzy_normalized = normalize_for_match(fuzzy_source);
        let fuzzy_tokenized = to_terms(fuzzy_source);
        Self {
            source,
            normalized: normalize_for_match(source),
            tokenized: to_terms(source),
            fuzzy_char_count: fuzzy_normalized.chars().count(),
            fuzzy_normalized,
            fuzzy_tokens: fuzzy_tokenized
                .split_whitespace()
                .map(|token| token.chars().collect())
                .collect(),
        }
    }
}

#[derive(Debug)]
struct PreparedCandidate<'a> {
    facts: &'a CandidateFacts,
    name: PreparedField<'a>,
    path: PreparedField<'a>,
    kind: PreparedField<'a>,
}

impl<'a> PreparedCandidate<'a> {
    fn new(facts: &'a CandidateFacts) -> Self {
        Self {
            facts,
            name: PreparedField::new(&facts.name),
            path: PreparedField::new(&facts.path),
            kind: PreparedField::new(&facts.kind),
        }
    }
}

fn prefix_by_chars(text: &str, max_chars: usize) -> &str {
    text.char_indices()
        .nth(max_chars)
        .map_or(text, |(end, _)| &text[..end])
}

#[derive(Debug)]
struct InternalRankedMatch<'a> {
    match_: RankedMatch,
    normalized_path: String,
    normalized_name: String,
    source_path: &'a str,
    source_name: &'a str,
}

impl InternalRankedMatch<'_> {
    fn add_highlights(&mut self, query_terms: &[QueryTerm]) {
        self.match_.highlight_path_ranges = highlight_ranges_for(self.source_path, query_terms);
        self.match_.highlight_name_ranges = highlight_ranges_for(self.source_name, query_terms);
        self.match_.highlight_path =
            highlight_html_from_ranges(self.source_path, &self.match_.highlight_path_ranges);
        self.match_.highlight_name =
            highlight_html_from_ranges(self.source_name, &self.match_.highlight_name_ranges);
    }
}

fn rank_candidates<'a>(
    query: &QuerySpec,
    candidates: impl IntoIterator<Item = (RetrievalStage, &'a CandidateFacts)>,
    fallback: &FuzzyFallbackPolicy,
    allow_fuzzy: bool,
    fuzzy_work_limit: usize,
) -> (
    Vec<InternalRankedMatch<'a>>,
    FuzzyWorkUsage,
    Option<SearchDiagnostic>,
) {
    let terms: Vec<_> = query.terms.iter().map(PreparedQueryTerm::new).collect();
    let mut fuzzy = FuzzyScorer::new(fuzzy_work_limit);
    let mut ranked = Vec::new();
    for (stage, candidate) in candidates {
        if let Some(candidate) = rank_candidate(
            &terms,
            candidate,
            stage,
            fallback,
            allow_fuzzy.then_some(&mut fuzzy),
        ) {
            ranked.push(candidate);
        }
    }
    let (usage, diagnostic) = fuzzy.finish();
    (ranked, usage, diagnostic)
}

fn rank_candidate_kind(
    terms: &[PreparedQueryTerm<'_>],
    candidate: &CandidateFacts,
    fallback: &FuzzyFallbackPolicy,
) -> Option<MatchKind> {
    if terms.is_empty() {
        return Some(MatchKind::None);
    }
    let candidate = PreparedCandidate::new(candidate);
    let mut match_kind = MatchKind::Exact;
    for (term_index, term) in terms.iter().enumerate() {
        let term_match = best_term_match(term_index, term, &candidate, fallback, None)?;
        match_kind = match_kind.worse_of(term_match.kind);
    }
    Some(match_kind)
}

fn candidate_matches_filters(query: &QuerySpec, candidate: &CandidateFacts) -> bool {
    if let Some(kind) = query.type_filter.as_deref()
        && normalize_for_match(&candidate.kind) != normalize_for_match(kind)
    {
        return false;
    }
    if let Some(prefix) = query.path_prefix.as_deref()
        && !normalize_for_match(&candidate.path).starts_with(&normalize_for_match(prefix))
    {
        return false;
    }
    true
}

fn rank_candidate<'a>(
    terms: &[PreparedQueryTerm<'_>],
    candidate: &'a CandidateFacts,
    stage: RetrievalStage,
    fallback: &FuzzyFallbackPolicy,
    mut fuzzy: Option<&mut FuzzyScorer>,
) -> Option<InternalRankedMatch<'a>> {
    let candidate = PreparedCandidate::new(candidate);
    let mut match_kind = MatchKind::Exact;
    let mut field_boost = 0u32;
    let mut fuzzy_score = 0i64;
    let mut term_explanations = Vec::with_capacity(terms.len());
    let mut used_fuzzy = false;

    for (term_index, term) in terms.iter().enumerate() {
        let term_match =
            best_term_match(term_index, term, &candidate, fallback, fuzzy.as_deref_mut())?;
        match_kind = match_kind.worse_of(term_match.kind);
        field_boost = field_boost.saturating_add(term_match.field.boost());
        fuzzy_score = fuzzy_score.saturating_add(term_match.fuzzy_score);
        used_fuzzy |= term_match.kind == MatchKind::Fuzzy;
        term_explanations.push(TermExplanation {
            term: term.source.text.clone(),
            quoted: term.source.quoted,
            kind: term_match.kind,
            field: term_match.field,
        });
    }

    if terms.is_empty() {
        match_kind = MatchKind::None;
    }
    let stable_key = candidate.facts.stable_key.clone();
    let retrieval_score = candidate.facts.retrieval_score;
    let source_path = candidate.path.source;
    let source_name = candidate.name.source;
    let normalized_path = candidate.path.normalized;
    let normalized_name = candidate.name.normalized;

    Some(InternalRankedMatch {
        match_: RankedMatch {
            rank: 0,
            stable_key,
            match_kind,
            ranking_signals: RankingSignals {
                field_boost,
                fuzzy_score,
                retrieval_stage: stage,
                retrieval_score,
            },
            explanation: MatchExplanation {
                terms: term_explanations,
                fuzzy_fallback: used_fuzzy,
            },
            highlight_path_ranges: Vec::new(),
            highlight_name_ranges: Vec::new(),
            highlight_path: None,
            highlight_name: None,
        },
        normalized_path,
        normalized_name,
        source_path,
        source_name,
    })
}

#[derive(Debug, Clone, Copy)]
struct TermMatch {
    kind: MatchKind,
    field: MatchField,
    fuzzy_score: i64,
}

fn best_term_match(
    term_index: usize,
    term: &PreparedQueryTerm<'_>,
    candidate: &PreparedCandidate<'_>,
    fallback: &FuzzyFallbackPolicy,
    fuzzy: Option<&mut FuzzyScorer>,
) -> Option<TermMatch> {
    let strict_evidence = candidate
        .facts
        .evidence
        .iter()
        .filter(|evidence| evidence.term_index == term_index)
        .filter(|evidence| evidence.kind != MatchKind::Fuzzy)
        .map(|evidence| TermMatch {
            kind: evidence.kind,
            field: evidence.field,
            fuzzy_score: 0,
        })
        .min_by(compare_term_match);
    let strict = [
        strict_match_in_field(term, &candidate.name, MatchField::Name),
        strict_match_in_field(term, &candidate.path, MatchField::Path),
        strict_match_in_field(term, &candidate.kind, MatchField::Kind),
        strict_evidence,
    ]
    .into_iter()
    .flatten()
    .min_by(compare_term_match);
    if strict.is_some() || term.source.quoted {
        return strict;
    }

    let fuzzy = fuzzy?;
    let evidence = candidate
        .facts
        .evidence
        .iter()
        .filter(|evidence| evidence.term_index == term_index && evidence.kind == MatchKind::Fuzzy)
        .map(|evidence| TermMatch {
            kind: evidence.kind,
            field: evidence.field,
            fuzzy_score: 0,
        })
        .min_by(compare_term_match);
    let name = fuzzy.match_field(term, &candidate.name, MatchField::Name, fallback);
    if fuzzy.is_exhausted() {
        return evidence;
    }
    if name.is_some() {
        return name;
    }
    let path = fuzzy.match_field(term, &candidate.path, MatchField::Path, fallback);
    if fuzzy.is_exhausted() {
        return evidence;
    }
    if path.is_some() {
        return path;
    }
    let kind = fuzzy.match_field(term, &candidate.kind, MatchField::Kind, fallback);
    if fuzzy.is_exhausted() {
        return evidence;
    }
    kind.or(evidence)
}

fn compare_term_match(left: &TermMatch, right: &TermMatch) -> Ordering {
    left.kind
        .ranking_cmp(right.kind)
        .then_with(|| right.field.boost().cmp(&left.field.boost()))
        .then_with(|| right.fuzzy_score.cmp(&left.fuzzy_score))
}

fn strict_match_in_field(
    term: &PreparedQueryTerm<'_>,
    field_text: &PreparedField<'_>,
    field: MatchField,
) -> Option<TermMatch> {
    strict_match_kind(term, field_text).map(|kind| TermMatch {
        kind,
        field,
        fuzzy_score: 0,
    })
}

fn strict_match_kind(
    term: &PreparedQueryTerm<'_>,
    field_text: &PreparedField<'_>,
) -> Option<MatchKind> {
    if term.source.quoted {
        return classify_normalized_match(&term.tokenized, &field_text.tokenized, false);
    }

    let raw_match = classify_normalized_match(&term.normalized, &field_text.normalized, true);
    let term_match = classify_normalized_match(&term.tokenized, &field_text.tokenized, true);
    match (raw_match, term_match) {
        (Some(left), Some(right)) => Some(if left.ranking_cmp(right).is_le() {
            left
        } else {
            right
        }),
        (Some(kind), None) | (None, Some(kind)) => Some(kind),
        (None, None) => None,
    }
}

fn classify_normalized_match(
    query: &str,
    field: &str,
    allow_abbreviation: bool,
) -> Option<MatchKind> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    if query == field {
        return Some(MatchKind::Exact);
    }
    if field.starts_with(query) {
        return Some(MatchKind::Prefix);
    }
    if field.split_whitespace().any(|token| token == query) {
        return Some(MatchKind::Token);
    }
    if field.contains(query) {
        return Some(MatchKind::Substring);
    }
    if allow_abbreviation && is_abbreviation_match(query, field) {
        return Some(MatchKind::Abbreviation);
    }
    None
}

fn is_abbreviation_match(query: &str, text: &str) -> bool {
    let query = query.split_whitespace().collect::<String>();
    if query.is_empty() {
        return false;
    }
    let initials = text
        .split_whitespace()
        .filter_map(|term| term.chars().next())
        .collect::<String>();
    initials.contains(&query)
}

fn fuzzy_char_count_is_eligible(char_count: usize, fallback: FuzzyFallbackPolicy) -> bool {
    char_count >= fallback.minimum_query_chars && char_count <= fallback.maximum_query_chars
}

fn fuzzy_retrieval_distance(
    quoted: bool,
    char_count: usize,
    fallback: FuzzyFallbackPolicy,
) -> Option<u8> {
    (!quoted && fuzzy_char_count_is_eligible(char_count, fallback))
        .then(|| {
            fallback
                .maximum_edit_distance
                .min(if char_count <= 7 { 1 } else { 2 })
                .min(u8::MAX as usize) as u8
        })
        .filter(|distance| *distance > 0)
}

fn query_term_supports_fuzzy_retrieval(term: &QueryTerm, fallback: FuzzyFallbackPolicy) -> bool {
    let normalized = to_terms(&term.text);
    normalized.split_whitespace().any(|token| {
        fuzzy_retrieval_distance(term.quoted, token.chars().count(), fallback).is_some()
    })
}

fn retrieval_evidence_is_valid(
    evidence: &RetrievalEvidence,
    query_term_count: usize,
    fuzzy_evidence_terms: &[bool],
) -> bool {
    evidence.term_index < query_term_count
        && evidence.field == MatchField::Content
        && evidence.kind != MatchKind::None
        && (evidence.kind != MatchKind::Fuzzy
            || fuzzy_evidence_terms
                .get(evidence.term_index)
                .copied()
                .unwrap_or(false))
}

fn should_use_fuzzy_fallback(
    query: &QuerySpec,
    strict_kinds: impl IntoIterator<Item = MatchKind>,
    fallback: FuzzyFallbackPolicy,
) -> bool {
    if !query.terms.iter().any(|term| {
        let chars = normalize_for_match(&term.text).chars().count();
        !term.quoted && fuzzy_char_count_is_eligible(chars, fallback)
    }) {
        return false;
    }

    let confident_matches = strict_kinds
        .into_iter()
        .filter(|kind| kind.meets(fallback.minimum_confident_kind))
        .count();
    confident_matches < fallback.minimum_confident_matches
}

struct FuzzyWorkBudget {
    usage: FuzzyWorkUsage,
    first_failed_attempt: Option<usize>,
}

impl FuzzyWorkBudget {
    const fn new(limit: usize) -> Self {
        Self {
            usage: FuzzyWorkUsage::new(limit),
            first_failed_attempt: None,
        }
    }

    fn charge(&mut self, units: usize) -> bool {
        if self.usage.exhausted {
            return false;
        }
        let Some(attempted) = self.usage.consumed.checked_add(units) else {
            self.exhaust(usize::MAX);
            return false;
        };
        if attempted > self.usage.limit {
            self.exhaust(attempted);
            return false;
        }
        self.usage.consumed = attempted;
        true
    }

    fn exhaust(&mut self, attempted: usize) {
        self.usage.exhausted = true;
        self.first_failed_attempt.get_or_insert(attempted);
    }

    fn finish(self) -> (FuzzyWorkUsage, Option<SearchDiagnostic>) {
        let diagnostic =
            self.first_failed_attempt
                .map(|attempted| SearchDiagnostic::FuzzyWorkLimitExceeded {
                    attempted,
                    limit: self.usage.limit,
                });
        (self.usage, diagnostic)
    }
}

struct FuzzyScorer {
    budget: FuzzyWorkBudget,
    matcher: SkimMatcherV2,
    previous: Vec<usize>,
    current: Vec<usize>,
}

impl FuzzyScorer {
    fn new(work_limit: usize) -> Self {
        Self {
            budget: FuzzyWorkBudget::new(work_limit),
            matcher: SkimMatcherV2::default(),
            previous: Vec::new(),
            current: Vec::new(),
        }
    }

    fn finish(self) -> (FuzzyWorkUsage, Option<SearchDiagnostic>) {
        self.budget.finish()
    }

    fn is_exhausted(&self) -> bool {
        self.budget.usage.exhausted
    }

    fn match_field(
        &mut self,
        query: &PreparedQueryTerm<'_>,
        field: &PreparedField<'_>,
        match_field: MatchField,
        fallback: &FuzzyFallbackPolicy,
    ) -> Option<TermMatch> {
        self.score(query, field, fallback)
            .map(|fuzzy_score| TermMatch {
                kind: MatchKind::Fuzzy,
                field: match_field,
                fuzzy_score,
            })
    }

    fn score(
        &mut self,
        query: &PreparedQueryTerm<'_>,
        field: &PreparedField<'_>,
        fallback: &FuzzyFallbackPolicy,
    ) -> Option<i64> {
        let query_chars = query.normalized_chars.len();
        if !fuzzy_char_count_is_eligible(query_chars, *fallback) {
            return None;
        }

        let Some(matcher_units) = matrix_work_units(query_chars, field.fuzzy_char_count) else {
            self.budget.exhaust(usize::MAX);
            return None;
        };
        if !self.budget.charge(matcher_units) {
            return None;
        }
        let matcher_score = self
            .matcher
            .fuzzy_match(&field.fuzzy_normalized, &query.normalized)
            .filter(|score| *score > 0);

        let maximum_distance =
            fallback
                .maximum_edit_distance
                .min(if query_chars <= 7 { 1 } else { 2 });
        let mut edit_score = None;
        for token in &field.fuzzy_tokens {
            let distance =
                self.edit_distance_with_limit(&query.normalized_chars, token, maximum_distance);
            if self.budget.usage.exhausted {
                return None;
            }
            if let Some(distance) = distance {
                let score = 10_000i64
                    .saturating_sub((distance as i64).saturating_mul(1_000))
                    .saturating_sub(token.len().abs_diff(query_chars) as i64);
                edit_score = Some(edit_score.map_or(score, |current: i64| current.max(score)));
            }
        }

        matcher_score.into_iter().chain(edit_score).max()
    }

    fn edit_distance_with_limit(
        &mut self,
        left: &[char],
        right: &[char],
        limit: usize,
    ) -> Option<usize> {
        if left.len().abs_diff(right.len()) > limit {
            return None;
        }
        let Some(work_units) = banded_edit_work_units(left.len(), right.len(), limit) else {
            self.budget.exhaust(usize::MAX);
            return None;
        };
        if !self.budget.charge(work_units) {
            return None;
        }

        let row_length = right.len().saturating_add(1);
        let outside_band = limit.saturating_add(1);
        self.previous.resize(row_length, outside_band);
        self.current.resize(row_length, outside_band);

        let initial_end = right.len().min(limit);
        for (index, value) in self.previous[..=initial_end].iter_mut().enumerate() {
            *value = index;
        }
        if initial_end < right.len() {
            self.previous[initial_end + 1] = outside_band;
        }

        for (left_index, left_char) in left.iter().enumerate() {
            let row = left_index + 1;
            let start = row.saturating_sub(limit).max(1);
            let end = row.saturating_add(limit).min(right.len());
            if start == 1 {
                self.current[0] = row;
            } else {
                self.current[start - 1] = outside_band;
            }
            for column in start..=end {
                let substitution =
                    self.previous[column - 1] + usize::from(*left_char != right[column - 1]);
                self.current[column] = substitution
                    .min(self.previous[column].saturating_add(1))
                    .min(self.current[column - 1].saturating_add(1));
            }
            if end < right.len() {
                self.current[end + 1] = outside_band;
            }
            std::mem::swap(&mut self.previous, &mut self.current);
        }

        (self.previous[right.len()] <= limit).then_some(self.previous[right.len()])
    }
}

fn matrix_work_units(query_chars: usize, field_chars: usize) -> Option<usize> {
    query_chars
        .checked_mul(field_chars)
        .and_then(|units| units.checked_add(query_chars))
        .and_then(|units| units.checked_add(field_chars))
}

fn banded_edit_work_units(left_len: usize, right_len: usize, limit: usize) -> Option<usize> {
    let mut units = right_len.min(limit).checked_add(1)?;
    for row in 1..=left_len {
        let start = row.saturating_sub(limit).max(1);
        let end = row.saturating_add(limit).min(right_len);
        if start <= end {
            units = units.checked_add(end - start + 1)?;
        }
    }
    Some(units)
}

fn compare_ranked(left: &InternalRankedMatch, right: &InternalRankedMatch) -> Ordering {
    left.match_
        .match_kind
        .ranking_cmp(right.match_.match_kind)
        .then_with(|| {
            right
                .match_
                .ranking_signals
                .field_boost
                .cmp(&left.match_.ranking_signals.field_boost)
        })
        .then_with(|| {
            right
                .match_
                .ranking_signals
                .fuzzy_score
                .cmp(&left.match_.ranking_signals.fuzzy_score)
        })
        .then_with(|| {
            left.match_
                .ranking_signals
                .retrieval_stage
                .ranking_priority()
                .cmp(
                    &right
                        .match_
                        .ranking_signals
                        .retrieval_stage
                        .ranking_priority(),
                )
        })
        .then_with(|| {
            if left.match_.ranking_signals.retrieval_stage
                == right.match_.ranking_signals.retrieval_stage
            {
                right
                    .match_
                    .ranking_signals
                    .retrieval_score
                    .cmp(&left.match_.ranking_signals.retrieval_score)
            } else {
                Ordering::Equal
            }
        })
        .then_with(|| left.normalized_path.cmp(&right.normalized_path))
        .then_with(|| left.normalized_name.cmp(&right.normalized_name))
        .then_with(|| left.match_.stable_key.cmp(&right.match_.stable_key))
}

pub fn highlight_html(text: &str, query_tokens: &[String]) -> Option<String> {
    let ranges = highlight_ranges(text, query_tokens);
    highlight_html_from_ranges(text, &ranges)
}

fn highlight_html_from_ranges(text: &str, ranges: &[HighlightRange]) -> Option<String> {
    if ranges.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(text.len() + ranges.len().saturating_mul(9));
    let mut cursor = 0usize;
    for &HighlightRange { start, end } in ranges {
        push_html_escaped(&mut out, text.get(cursor..start)?);
        out.push_str("<em>");
        push_html_escaped(&mut out, text.get(start..end)?);
        out.push_str("</em>");
        cursor = end;
    }
    push_html_escaped(&mut out, text.get(cursor..)?);
    Some(out)
}

fn push_html_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
}

pub fn highlight_ranges(text: &str, query_tokens: &[String]) -> Vec<HighlightRange> {
    highlight_ranges_for(text, query_tokens)
}

fn highlight_ranges_for<T: AsRef<str>>(text: &str, query_tokens: &[T]) -> Vec<HighlightRange> {
    if text.len() > MAX_HIGHLIGHT_FIELD_BYTES
        || query_tokens
            .iter()
            .try_fold(0usize, |total, token| {
                total.checked_add(token.as_ref().len())
            })
            .is_none_or(|total| total > MAX_HIGHLIGHT_QUERY_BYTES)
    {
        return Vec::new();
    }
    let normalized = NormalizedText::new(text);
    let mut ranges = Vec::new();

    for token in query_tokens
        .iter()
        .map(AsRef::as_ref)
        .filter(|token| !token.is_empty())
    {
        let needle = normalize_for_match(token);
        if needle.is_empty() {
            continue;
        }
        let Some(start) = normalized.text.find(&needle) else {
            continue;
        };
        let end = start + needle.len();
        let Some(range) = normalized.source_range(start, end) else {
            continue;
        };
        if ranges.iter().any(|existing: &HighlightRange| {
            range.end > existing.start && range.start < existing.end
        }) {
            continue;
        }
        ranges.push(range);
    }

    ranges.sort_by_key(|range| range.start);
    ranges
}

#[derive(Debug)]
struct NormalizedText {
    text: String,
    source_start: Vec<usize>,
    source_end: Vec<usize>,
}

impl NormalizedText {
    fn new(source: &str) -> Self {
        let normalized_full = normalize_for_match(source);
        let mut text = String::with_capacity(normalized_full.len());
        let mut source_start = Vec::new();
        let mut source_end = Vec::new();

        let mut cluster_start = 0usize;
        for (start, ch) in source.char_indices().skip(1) {
            if canonical_combining_class(ch) == 0 {
                push_normalized_cluster(
                    source,
                    cluster_start,
                    start,
                    &mut text,
                    &mut source_start,
                    &mut source_end,
                );
                cluster_start = start;
            }
        }
        if !source.is_empty() {
            push_normalized_cluster(
                source,
                cluster_start,
                source.len(),
                &mut text,
                &mut source_start,
                &mut source_end,
            );
        }

        if text != normalized_full {
            text = normalized_full;
            source_start = vec![0; text.len()];
            source_end = vec![source.len(); text.len()];
        }

        Self {
            text,
            source_start,
            source_end,
        }
    }

    fn source_range(&self, start: usize, end: usize) -> Option<HighlightRange> {
        if start >= end || end > self.text.len() {
            return None;
        }
        Some(HighlightRange {
            start: *self.source_start.get(start)?,
            end: *self.source_end.get(end - 1)?,
        })
    }
}

fn push_normalized_cluster(
    source: &str,
    start: usize,
    end: usize,
    normalized: &mut String,
    source_start: &mut Vec<usize>,
    source_end: &mut Vec<usize>,
) {
    let normalized_start = normalized.len();
    normalized.extend(source[start..end].nfkc().flat_map(char::to_lowercase));
    let normalized_len = normalized.len() - normalized_start;
    source_start.extend(std::iter::repeat_n(start, normalized_len));
    source_end.extend(std::iter::repeat_n(end, normalized_len));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_split_paths_camel_case_and_digits() {
        assert_eq!(
            to_terms("Assets/UI/MainMenu/Button2D.prefab"),
            "assets ui main menu button 2 d prefab"
        );
    }

    #[test]
    fn abbreviation_matches_camel_case_terms() {
        let outcome = SearchPolicy::default()
            .prepare(SearchRequest::new("mm", 10))
            .execute([CandidateFacts::new(
                "main-menu",
                "MainMenu",
                "Assets/UI/MainMenu.prefab",
                "Prefab",
                1,
            )]);
        assert_eq!(outcome.matches[0].match_kind, MatchKind::Abbreviation);
    }

    #[test]
    fn parse_query_extracts_and_canonicalizes_filters() {
        let query = parse_query("t:prefab in:\"Assets/UI\" \"Start Button\"");
        assert_eq!(query.type_filter.as_deref(), Some("Prefab"));
        assert_eq!(query.path_prefix.as_deref(), Some("Assets/UI"));
        assert_eq!(query.terms.len(), 1);
        assert_eq!(query.terms[0].text, "Start Button");
        assert!(query.terms[0].quoted);
    }

    #[test]
    fn highlight_html_wraps_tokens() {
        let output = highlight_html("Assets/UI/Button.prefab", &[String::from("ui")]).unwrap();
        assert!(output.contains("<em>UI</em>") || output.contains("<em>ui</em>"));
    }

    #[test]
    fn banded_edit_distance_preserves_limit_boundaries() {
        let chars = |value: &str| value.chars().collect::<Vec<_>>();
        let mut scorer = FuzzyScorer::new(10_000);

        assert_eq!(
            scorer.edit_distance_with_limit(&chars("kitten"), &chars("sitten"), 1),
            Some(1)
        );
        assert_eq!(
            scorer.edit_distance_with_limit(&chars("kitten"), &chars("kitten"), 0),
            Some(0)
        );
        assert_eq!(
            scorer.edit_distance_with_limit(&chars("kitten"), &chars("sitting"), 2),
            None
        );
        assert_eq!(
            scorer.edit_distance_with_limit(&[], &chars("ab"), 2),
            Some(2)
        );
        assert_eq!(
            scorer.edit_distance_with_limit(&chars("按钮"), &chars("按鈕"), 1),
            Some(1)
        );
        assert!(scorer.budget.usage.consumed <= scorer.budget.usage.limit);
    }

    #[test]
    fn banded_edit_distance_matches_full_dp_for_small_inputs() {
        fn strings(max_length: usize) -> Vec<Vec<char>> {
            let mut values = vec![Vec::new()];
            for length in 1..=max_length {
                for bits in 0..(1usize << length) {
                    values.push(
                        (0..length)
                            .map(|index| if bits & (1 << index) == 0 { 'a' } else { 'b' })
                            .collect(),
                    );
                }
            }
            values
        }

        fn full_distance(left: &[char], right: &[char]) -> usize {
            let mut previous: Vec<_> = (0..=right.len()).collect();
            let mut current = vec![0; right.len() + 1];
            for (left_index, left_char) in left.iter().enumerate() {
                current[0] = left_index + 1;
                for (right_index, right_char) in right.iter().enumerate() {
                    current[right_index + 1] = (previous[right_index]
                        + usize::from(left_char != right_char))
                    .min(previous[right_index + 1] + 1)
                    .min(current[right_index] + 1);
                }
                std::mem::swap(&mut previous, &mut current);
            }
            previous[right.len()]
        }

        let values = strings(4);
        let mut scorer = FuzzyScorer::new(1_000_000);
        for left in &values {
            for right in &values {
                let distance = full_distance(left, right);
                for limit in 0..=2 {
                    assert_eq!(
                        scorer.edit_distance_with_limit(left, right, limit),
                        (distance <= limit).then_some(distance),
                        "left={left:?}, right={right:?}, limit={limit}"
                    );
                }
            }
        }
    }

    #[test]
    fn fuzzy_work_budget_fails_closed_on_arithmetic_overflow() {
        let mut budget = FuzzyWorkBudget::new(usize::MAX);

        assert!(budget.charge(1));
        assert!(!budget.charge(usize::MAX));
        let (usage, diagnostic) = budget.finish();
        assert!(usage.exhausted);
        assert_eq!(usage.consumed, 1);
        assert_eq!(
            diagnostic,
            Some(SearchDiagnostic::FuzzyWorkLimitExceeded {
                attempted: usize::MAX,
                limit: usize::MAX,
            })
        );
    }
}
