use std::collections::TryReserveError;
use std::convert::Infallible;
use std::error::Error as StdError;
use std::fmt;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

const MAX_HIGHLIGHT_FIELD_BYTES: usize = 32 * 1024;
const MAX_HIGHLIGHT_QUERY_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightRange {
    pub start: usize,
    pub end: usize,
}

pub fn normalize_for_match(input: &str) -> String {
    input.nfkc().collect::<String>().to_lowercase()
}

/// Failure produced while reserving the normalized term output.
#[derive(Debug)]
pub enum TryToTermsError<E> {
    ReserveHook {
        requested: usize,
        source: E,
    },
    Allocation {
        requested: usize,
        source: TryReserveError,
    },
}

impl<E: fmt::Display> fmt::Display for TryToTermsError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReserveHook { requested, .. } => write!(
                formatter,
                "term output reserve hook rejected a {requested}-byte result layout"
            ),
            Self::Allocation { requested, .. } => write!(
                formatter,
                "failed to reserve a {requested}-byte term output layout"
            ),
        }
    }
}

impl<E: StdError + 'static> StdError for TryToTermsError<E> {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::ReserveHook { source, .. } => Some(source),
            Self::Allocation { source, .. } => Some(source),
        }
    }
}

pub fn to_terms(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let result = tokenize_terms(input, |ch| {
        out.push(ch);
        Ok::<(), Infallible>(())
    });
    match result {
        Ok(()) => out,
        Err(never) => match never {},
    }
}

/// Normalizes `input` into search terms using fallible output allocation.
///
/// Before each reserve, `before_reserve` receives the complete minimum byte layout requested for
/// the resulting `String`, not an allocator-rounded capacity or a growth delta.
pub fn try_to_terms<E>(
    input: &str,
    mut before_reserve: impl FnMut(usize) -> Result<(), E>,
) -> Result<String, TryToTermsError<E>> {
    let mut out = String::new();
    try_reserve_terms(&mut out, input.len(), &mut before_reserve)?;
    tokenize_terms(input, |ch| {
        try_reserve_terms(&mut out, ch.len_utf8(), &mut before_reserve)?;
        out.push(ch);
        Ok(())
    })?;
    Ok(out)
}

fn try_reserve_terms<E>(
    out: &mut String,
    additional: usize,
    before_reserve: &mut impl FnMut(usize) -> Result<(), E>,
) -> Result<(), TryToTermsError<E>> {
    if additional <= out.capacity().saturating_sub(out.len()) {
        return Ok(());
    }
    let requested = out.len().saturating_add(additional);
    before_reserve(requested)
        .map_err(|source| TryToTermsError::ReserveHook { requested, source })?;
    out.try_reserve(additional)
        .map_err(|source| TryToTermsError::Allocation { requested, source })
}

fn tokenize_terms<E>(input: &str, mut push: impl FnMut(char) -> Result<(), E>) -> Result<(), E> {
    let mut normalized = input.nfkc().peekable();
    let mut previous: Option<char> = None;
    let mut has_output = false;
    let mut pending_boundary = false;

    while let Some(ch) = normalized.next() {
        if is_term_separator(ch) {
            pending_boundary = has_output;
            previous = None;
            continue;
        }

        let next = normalized.peek().copied();
        if let Some(previous) = previous {
            let camel_boundary = ch.is_uppercase()
                && (previous.is_lowercase()
                    || (previous.is_uppercase() && next.is_some_and(|next| next.is_lowercase())));
            let digit_boundary = ch.is_numeric() != previous.is_numeric();
            if camel_boundary || digit_boundary {
                pending_boundary = has_output;
            }
        }

        for lower in ch.to_lowercase() {
            if pending_boundary {
                push(' ')?;
                pending_boundary = false;
            }
            push(lower)?;
            has_output = true;
        }
        previous = Some(ch);
    }

    Ok(())
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

pub fn highlight_ranges(text: &str, query_tokens: &[String]) -> Vec<HighlightRange> {
    highlight_ranges_for(text, query_tokens)
}

pub(super) fn highlight_ranges_for<T: AsRef<str>>(
    text: &str,
    query_tokens: &[T],
) -> Vec<HighlightRange> {
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
    use std::convert::Infallible;

    use super::*;

    #[test]
    fn terms_split_paths_camel_case_and_digits() {
        assert_eq!(
            to_terms("Assets/UI/MainMenu/Button2D.prefab"),
            "assets ui main menu button 2 d prefab"
        );
    }

    #[test]
    fn fallible_terms_match_the_previous_tokenizer_semantics() {
        assert_eq!(to_terms("ＦｏｏＢａｒ１２/Kelvin"), "foo bar 12 kelvin");
        assert_eq!(to_terms("Cafe\u{301}/İstanbul"), "café i\u{307}stanbul");

        let corpus = [
            "",
            "  /--__::;;,,.()[]{}\"'  ",
            "Assets/UI/MainMenu/Button2D.prefab",
            "XMLHttpRequest42D",
            "ＦｏｏＢａｒ１２/Kelvin",
            "Cafe\u{301}/İstanbul",
            "A___B---C...D///E\\\\F",
            "JSON2XML99Bottles",
            "Straße_ΔValue٣D",
            "trailing/separators///",
        ];

        for input in corpus {
            let expected = legacy_to_terms(input);
            assert_eq!(to_terms(input), expected, "infallible input: {input:?}");
            assert_eq!(
                try_to_terms(input, |_| Ok::<(), Infallible>(())).unwrap(),
                expected,
                "fallible input: {input:?}"
            );
        }
    }

    #[test]
    fn reserve_hook_failure_precedes_the_first_allocation() {
        #[derive(Debug, PartialEq, Eq)]
        struct Rejected;

        impl fmt::Display for Rejected {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("rejected")
            }
        }

        impl StdError for Rejected {}

        let mut calls = 0;
        let error = try_to_terms("Button2D", |requested| {
            calls += 1;
            assert_eq!(requested, "Button2D".len());
            Err(Rejected)
        })
        .unwrap_err();

        assert_eq!(calls, 1);
        assert!(matches!(
            error,
            TryToTermsError::ReserveHook {
                requested: 8,
                source: Rejected
            }
        ));
    }

    #[test]
    fn nfkc_growth_reports_the_second_requested_layout_and_error_source() {
        #[derive(Debug, PartialEq, Eq)]
        struct Rejected;

        impl fmt::Display for Rejected {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("rejected")
            }
        }

        impl StdError for Rejected {}

        let input = "\u{fdfa}";
        let mut requests = Vec::new();
        let error = try_to_terms(input, |requested| {
            requests.push(requested);
            if requests.len() == 2 {
                Err(Rejected)
            } else {
                Ok(())
            }
        })
        .unwrap_err();

        assert_eq!(requests.first(), Some(&input.len()));
        assert_eq!(requests.len(), 2);
        assert!(requests[1] > input.len());
        assert!(matches!(
            &error,
            TryToTermsError::ReserveHook {
                requested,
                source: Rejected
            } if *requested == requests[1]
        ));
        assert!(
            StdError::source(&error)
                .and_then(|source| source.downcast_ref::<Rejected>())
                .is_some()
        );
    }

    fn legacy_to_terms(input: &str) -> String {
        let normalized: Vec<char> = input.nfkc().collect();
        let mut out = String::with_capacity(input.len());
        let mut previous: Option<char> = None;

        for (index, ch) in normalized.iter().copied().enumerate() {
            if is_term_separator(ch) {
                legacy_push_term_boundary(&mut out);
                previous = None;
                continue;
            }

            let next = normalized.get(index + 1).copied();
            if let Some(previous) = previous {
                let camel_boundary = ch.is_uppercase()
                    && (previous.is_lowercase()
                        || (previous.is_uppercase()
                            && next.is_some_and(|next| next.is_lowercase())));
                let digit_boundary = ch.is_numeric() != previous.is_numeric();
                if camel_boundary || digit_boundary {
                    legacy_push_term_boundary(&mut out);
                }
            }

            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            previous = Some(ch);
        }

        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn legacy_push_term_boundary(out: &mut String) {
        if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
    }
}
