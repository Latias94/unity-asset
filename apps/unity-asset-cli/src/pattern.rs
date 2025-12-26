pub(crate) fn container_asset_path_matches_ci(asset_path: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }

    let pattern_lc = pattern.to_ascii_lowercase();
    let asset_path_lc = asset_path.to_ascii_lowercase();

    if pattern_lc.contains('*') || pattern_lc.contains('?') {
        let glob = parse_glob_pattern(&pattern_lc);
        glob_match(&glob, &asset_path_lc)
    } else {
        asset_path_lc.contains(&pattern_lc)
    }
}

#[derive(Debug, Clone, Copy)]
enum GlobToken {
    Star,
    AnyChar,
    Literal(u8),
}

fn parse_glob_pattern(pattern: &str) -> Vec<GlobToken> {
    let bytes = pattern.as_bytes();
    let mut out = Vec::new();

    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                if i + 1 < bytes.len() {
                    out.push(GlobToken::Literal(bytes[i + 1]));
                    i += 2;
                } else {
                    out.push(GlobToken::Literal(b'\\'));
                    i += 1;
                }
            }
            b'*' => {
                if !matches!(out.last(), Some(GlobToken::Star)) {
                    out.push(GlobToken::Star);
                }
                i += 1;
            }
            b'?' => {
                out.push(GlobToken::AnyChar);
                i += 1;
            }
            other => {
                out.push(GlobToken::Literal(other));
                i += 1;
            }
        }
    }

    out
}

fn glob_match(tokens: &[GlobToken], text: &str) -> bool {
    let text = text.as_bytes();

    let mut token_index = 0usize;
    let mut text_index = 0usize;

    let mut last_star: Option<usize> = None;
    let mut star_text_index = 0usize;

    while text_index < text.len() {
        match tokens.get(token_index) {
            Some(GlobToken::Literal(ch)) if *ch == text[text_index] => {
                token_index += 1;
                text_index += 1;
            }
            Some(GlobToken::AnyChar) => {
                token_index += 1;
                text_index += 1;
            }
            Some(GlobToken::Star) => {
                last_star = Some(token_index);
                token_index += 1;
                star_text_index = text_index;
            }
            _ => {
                if let Some(star) = last_star {
                    star_text_index += 1;
                    text_index = star_text_index;
                    token_index = star + 1;
                } else {
                    return false;
                }
            }
        }
    }

    while matches!(tokens.get(token_index), Some(GlobToken::Star)) {
        token_index += 1;
    }

    token_index == tokens.len()
}

