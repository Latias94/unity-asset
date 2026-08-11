//! Public API contract for the `unity-asset-search-core` package.

pub use unity_asset_search_core::{
    CandidateFacts, FuzzyFallbackPolicy, MatchExplanation, PreparedSearch, QuerySpec,
    RetrievalEvidence, SearchDiagnostic, SearchKind, SearchLimits, SearchOutcome, SearchPolicy,
    SearchRequest, highlight_ranges, normalize_for_match, parse_query, try_to_terms,
};
