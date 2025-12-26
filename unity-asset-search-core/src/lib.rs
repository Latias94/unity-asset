use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

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
}
