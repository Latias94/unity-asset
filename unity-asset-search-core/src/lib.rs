use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuerySpec {
    pub raw: String,
    pub free_text: String,
    pub type_filter: Option<String>,
    pub path_prefix: Option<String>,
    pub tokens: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchKind {
    Exact = 0,
    Prefix = 1,
    Substring = 2,
    Abbreviation = 3,
    Fuzzy = 4,
    None = 5,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedScore {
    pub kind: MatchKind,
    pub fuzzy_score: i64,
}

pub fn normalize_for_match(input: &str) -> String {
    input.nfkc().collect::<String>().to_lowercase()
}

pub fn parse_query(input: &str) -> QuerySpec {
    let raw = input.to_string();
    let mut type_filter = None;
    let mut path_prefix = None;

    let mut tokens = Vec::new();
    let mut quoted = Vec::new();

    let mut buf = String::new();
    let mut in_quotes = false;
    let mut token_was_quoted = false;

    for ch in input.chars() {
        if ch == '"' {
            in_quotes = !in_quotes;
            if in_quotes {
                token_was_quoted = true;
            }
            continue;
        }

        if ch.is_whitespace() && !in_quotes {
            if !buf.is_empty() {
                tokens.push(buf.clone());
                quoted.push(token_was_quoted);
                buf.clear();
                token_was_quoted = false;
            }
            continue;
        }

        buf.push(ch);
    }
    if !buf.is_empty() {
        tokens.push(buf);
        quoted.push(token_was_quoted);
    }

    let mut free_tokens = Vec::new();
    let mut highlight_tokens = Vec::new();

    for (token, was_quoted) in tokens.into_iter().zip(quoted.into_iter()) {
        if let Some(value) = token
            .strip_prefix("t:")
            .or_else(|| token.strip_prefix("type:"))
        {
            let value = value.trim().trim_matches('"').to_string();
            if !value.is_empty() {
                type_filter = Some(value);
            }
            continue;
        }
        if let Some(value) = token.strip_prefix("in:") {
            let value = value.trim().trim_matches('"').to_string();
            if !value.is_empty() {
                path_prefix = Some(value);
            }
            continue;
        }

        if was_quoted {
            free_tokens.push(format!("\"{token}\""));
        } else {
            free_tokens.push(token.clone());
        }

        if !token.is_empty() {
            highlight_tokens.push(token);
        }
    }

    let free_text = free_tokens.join(" ").trim().to_string();

    QuerySpec {
        raw,
        free_text,
        type_filter,
        path_prefix,
        tokens: highlight_tokens,
    }
}

pub fn to_terms(input: &str) -> String {
    let mut out = String::with_capacity(input.len());

    let mut prev_is_boundary = true;
    let mut prev_is_lower = false;
    let mut prev_is_digit = false;

    for ch in input.nfkc() {
        let is_sep = matches!(
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
        );
        let is_boundary = is_sep || ch.is_whitespace();

        if is_boundary {
            if !prev_is_boundary {
                out.push(' ');
            }
            prev_is_boundary = true;
            prev_is_lower = false;
            prev_is_digit = false;
            continue;
        }

        let is_upper = ch.is_uppercase();
        let is_lower = ch.is_lowercase();
        let is_digit = ch.is_ascii_digit();

        if !prev_is_boundary && is_upper && prev_is_lower {
            out.push(' ');
        }
        if !prev_is_boundary && is_digit && !prev_is_digit {
            out.push(' ');
        }

        for lower in ch.to_lowercase() {
            out.push(lower);
        }
        prev_is_boundary = false;
        prev_is_lower = is_lower;
        prev_is_digit = is_digit;
    }

    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn rank_match(query: &str, name: &str, path: &str) -> RankedScore {
    let query_norm = normalize_for_match(query).trim().to_string();
    if query_norm.is_empty() {
        return RankedScore {
            kind: MatchKind::None,
            fuzzy_score: 0,
        };
    }

    let name_norm = normalize_for_match(name);
    let path_norm = normalize_for_match(path);

    if query_norm == name_norm || query_norm == path_norm {
        return RankedScore {
            kind: MatchKind::Exact,
            fuzzy_score: i64::MAX,
        };
    }

    if name_norm.starts_with(&query_norm) || path_norm.starts_with(&query_norm) {
        return RankedScore {
            kind: MatchKind::Prefix,
            fuzzy_score: i64::MAX / 2,
        };
    }

    if name_norm.contains(&query_norm) || path_norm.contains(&query_norm) {
        return RankedScore {
            kind: MatchKind::Substring,
            fuzzy_score: i64::MAX / 4,
        };
    }

    if is_abbreviation_match(&query_norm, name) || is_abbreviation_match(&query_norm, path) {
        return RankedScore {
            kind: MatchKind::Abbreviation,
            fuzzy_score: i64::MAX / 8,
        };
    }

    let matcher = SkimMatcherV2::default();
    let fuzzy_score = matcher
        .fuzzy_match(&name_norm, &query_norm)
        .into_iter()
        .chain(matcher.fuzzy_match(&path_norm, &query_norm))
        .max()
        .unwrap_or(0);

    RankedScore {
        kind: if fuzzy_score > 0 {
            MatchKind::Fuzzy
        } else {
            MatchKind::None
        },
        fuzzy_score,
    }
}

pub fn highlight_html(text: &str, query_tokens: &[String]) -> Option<String> {
    let ranges = highlight_ranges(text, query_tokens);
    if ranges.is_empty() {
        return None;
    }
    if !text.is_ascii() || ranges.iter().any(|r| !text.is_char_boundary(r.start) || !text.is_char_boundary(r.end)) {
        return None;
    }

    let mut out = String::with_capacity(text.len() + ranges.len() * 9);
    let mut cursor = 0usize;
    for HighlightRange { start, end } in ranges {
        if let Some(prefix) = text.get(cursor..start) {
            out.push_str(prefix);
        }
        out.push_str("<em>");
        if let Some(mid) = text.get(start..end) {
            out.push_str(mid);
        }
        out.push_str("</em>");
        cursor = end;
    }
    if let Some(rest) = text.get(cursor..) {
        out.push_str(rest);
    }
    Some(out)
}

pub fn highlight_ranges(text: &str, query_tokens: &[String]) -> Vec<HighlightRange> {
    let tokens: Vec<&str> = query_tokens
        .iter()
        .map(String::as_str)
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    if !text.is_ascii() || tokens.iter().any(|t| !t.is_ascii()) {
        return Vec::new();
    }

    let hay = text.as_bytes();
    let hay_lower: Vec<u8> = hay.iter().map(|b| b.to_ascii_lowercase()).collect();

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for token in tokens {
        let needle_lower: Vec<u8> = token
            .as_bytes()
            .iter()
            .map(|b| b.to_ascii_lowercase())
            .collect();
        if needle_lower.is_empty() {
            continue;
        }

        let Some((start, end)) = find_subslice(&hay_lower, &needle_lower) else {
            continue;
        };

        if ranges.iter().any(|(s, e)| !(end <= *s || start >= *e)) {
            continue;
        }
        ranges.push((start, end));
    }

    if ranges.is_empty() {
        return Vec::new();
    }
    ranges.sort_by_key(|(s, _)| *s);

    ranges
        .into_iter()
        .map(|(start, end)| HighlightRange { start, end })
        .collect()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<(usize, usize)> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    for i in 0..=(haystack.len() - needle.len()) {
        if haystack[i..i + needle.len()] == *needle {
            return Some((i, i + needle.len()));
        }
    }
    None
}

fn is_abbreviation_match(query_norm: &str, text: &str) -> bool {
    if query_norm.is_empty() {
        return false;
    }

    let terms = to_terms(text);
    let initials = terms
        .split_whitespace()
        .filter_map(|t| t.chars().next())
        .collect::<String>();

    initials.contains(query_norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_split_paths() {
        assert_eq!(
            to_terms("Assets/UI/MainMenu/Button.prefab"),
            "assets ui main menu button prefab"
        );
    }

    #[test]
    fn ranking_prefers_prefix() {
        let a = rank_match("but", "button", "assets/ui/button.prefab");
        assert_eq!(a.kind, MatchKind::Prefix);
    }

    #[test]
    fn abbreviation_matches_camel_case_terms() {
        let a = rank_match("mm", "MainMenu", "Assets/UI/MainMenu.prefab");
        assert_eq!(a.kind, MatchKind::Abbreviation);
    }

    #[test]
    fn parse_query_extracts_filters() {
        let q = parse_query("t:prefab in:\"Assets/UI\" \"Start Button\"");
        assert_eq!(q.type_filter.as_deref(), Some("prefab"));
        assert_eq!(q.path_prefix.as_deref(), Some("Assets/UI"));
        assert_eq!(q.free_text, "\"Start Button\"");
    }

    #[test]
    fn highlight_html_wraps_tokens() {
        let out = highlight_html("Assets/UI/Button.prefab", &[String::from("ui")]).unwrap();
        assert!(out.contains("<em>UI</em>") || out.contains("<em>ui</em>"));
    }
}
