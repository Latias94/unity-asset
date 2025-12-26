# ADR 0002: Fuzzy search and ranking strategy

- Status: Proposed
- Date: 2025-12-26
- Current repo HEAD at time of writing: `f4a6bd6bbc3228f1b34a9c4b534e23e06836f286`

## Context

We want a search experience that feels "IDE-like":

- results appear as the user types (low latency)
- ranking is predictable and stable
- fuzzy matching tolerates incomplete input and minor typos
- users should understand why a result matched (highlighting)

Pure full-text ranking (BM25) is not enough for path-heavy, symbol-heavy queries, while pure fuzzy scoring can be noisy.

## Decision

Use a two-stage ranking strategy with a strict default and a controlled fuzzy fallback.

### Stage 1: Retrieval (index-side)

- Use a full-text index (e.g. Tantivy) to retrieve top `N` candidates (e.g. 200).
- Index multiple fields with different weights, favoring user-visible metadata:
  - `name`, `path`, `type`, `component` (high weight)
  - `content` (lower weight)
- Tokenization rules must match real Unity queries:
  - split on `/`, `\\`, `.`, `-`, `_`, whitespace
  - camelCase and digit boundaries
  - Unicode normalization (NFKC) and lowercase

### Stage 2: Re-ranking (policy-side)

Re-rank the retrieved candidates using a deterministic fuzzy scorer on `name/path`:

1. exact match
2. prefix match
3. substring match
4. abbreviation match (camelCase initials)
5. typo-tolerant fuzzy match (limited)

Combine the fuzzy score with field weights from Stage 1 to avoid noisy content matches dominating the top results.

### Fallback mode

If strict ranking yields low confidence (e.g. top results are weak), optionally expand the fuzzy tolerance:

- enable a more aggressive subsequence match
- allow limited edit distance for longer tokens

Fallback should be explicit (config flag or automatically triggered only when confidence is low).

## Consequences

- Pros:
  - Feels like an IDE: fast, stable, and explainable.
  - Avoids "random-looking" results caused by overly aggressive fuzzy matching.
  - Keeps index implementation simpler: complex heuristics live in the ranking policy layer.
- Cons:
  - Requires careful tuning and evaluation on real project datasets.
  - Two-stage ranking increases implementation complexity compared to a single scorer.

## Alternatives considered

1. BM25 only
   - Often fails on partial and abbreviated queries common in editor workflows.
2. Fuzzy scoring only
   - Too noisy without strong field/semantic constraints.
3. Full query-time scanning (no index)
   - Not feasible for interactive search on large projects.

