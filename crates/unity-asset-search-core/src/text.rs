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

pub fn highlight_html(text: &str, query_tokens: &[String]) -> Option<String> {
    let ranges = highlight_ranges(text, query_tokens);
    highlight_html_from_ranges(text, &ranges)
}

pub(super) fn highlight_html_from_ranges(text: &str, ranges: &[HighlightRange]) -> Option<String> {
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
    use super::*;

    #[test]
    fn terms_split_paths_camel_case_and_digits() {
        assert_eq!(
            to_terms("Assets/UI/MainMenu/Button2D.prefab"),
            "assets ui main menu button 2 d prefab"
        );
    }

    #[test]
    fn highlight_html_wraps_tokens() {
        let output = highlight_html("Assets/UI/Button.prefab", &[String::from("ui")]).unwrap();
        assert!(output.contains("<em>UI</em>") || output.contains("<em>ui</em>"));
    }
}
