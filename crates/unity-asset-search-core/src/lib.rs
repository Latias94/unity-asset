mod policy;
mod text;

pub use policy::{
    ABSOLUTE_MAX_CANDIDATES, CandidateFacts, CandidateField, FuzzyFallbackPolicy, FuzzyWorkUsage,
    MatchCount, MatchCountRelation, MatchExplanation, MatchField, MatchKind, PreparedSearch,
    QuerySpec, QueryTerm, RankedMatch, RankingSignals, RetrievalEvidence, RetrievalFieldPolicy,
    RetrievalStage, RetrievalTerm, SearchDiagnostic, SearchDiagnosticSeverity, SearchKind,
    SearchLimits, SearchOutcome, SearchPolicy, SearchRequest, TermExplanation, parse_query,
};
pub use text::{
    HighlightRange, TryToTermsError, highlight_html, highlight_ranges, normalize_for_match,
    to_terms, try_to_terms,
};
