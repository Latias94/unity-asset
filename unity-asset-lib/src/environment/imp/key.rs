use std::path::PathBuf;
use std::str::FromStr;

use super::{BinaryObjectKey, BinarySource, BinarySourceKind};

impl std::fmt::Display for BinaryObjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.source_kind {
            BinarySourceKind::SerializedFile => "serialized",
            BinarySourceKind::AssetBundle => "bundle",
        };
        let asset_index = self
            .asset_index
            .map(|i| i.to_string())
            .unwrap_or_else(|| "-".to_string());

        let (outer, entry) = match &self.source {
            BinarySource::Path(p) => (p.to_string_lossy().to_string(), String::new()),
            BinarySource::WebEntry {
                web_path,
                entry_name,
            } => (web_path.to_string_lossy().to_string(), entry_name.clone()),
        };
        write!(
            f,
            "bok2|{}|{}|{}|{}|{}|{}|{}",
            kind,
            asset_index,
            self.path_id,
            outer.as_bytes().len(),
            outer,
            entry.as_bytes().len(),
            entry
        )
    }
}

impl FromStr for BinaryObjectKey {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.starts_with("bok2|") {
            return parse_bok2(s);
        }
        if s.starts_with("bok1|") {
            return parse_bok1(s);
        }
        Err("invalid key prefix (expected: bok1|... or bok2|...)".to_string())
    }
}

fn parse_kind(kind: &str) -> std::result::Result<BinarySourceKind, String> {
    match kind {
        "bundle" => Ok(BinarySourceKind::AssetBundle),
        "serialized" => Ok(BinarySourceKind::SerializedFile),
        other => Err(format!("unknown kind: {}", other)),
    }
}

fn parse_asset_index(asset_index: &str) -> std::result::Result<Option<usize>, String> {
    if asset_index == "-" || asset_index.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        asset_index
            .parse::<usize>()
            .map_err(|e| format!("invalid asset_index: {}", e))?,
    ))
}

fn parse_bok1(s: &str) -> std::result::Result<BinaryObjectKey, String> {
    let prefix = "bok1|";
    let mut rest = &s[prefix.len()..];
    let (kind, r) = split_once(rest, '|').ok_or_else(|| "missing kind".to_string())?;
    rest = r;
    let (asset_index, r) =
        split_once(rest, '|').ok_or_else(|| "missing asset_index".to_string())?;
    rest = r;
    let (path_id, r) = split_once(rest, '|').ok_or_else(|| "missing path_id".to_string())?;
    rest = r;
    let (path_len, path) =
        split_once(rest, '|').ok_or_else(|| "missing path_len/path".to_string())?;

    let source_kind = parse_kind(kind)?;
    let asset_index = parse_asset_index(asset_index)?;
    let path_id = path_id
        .parse::<i64>()
        .map_err(|e| format!("invalid path_id: {}", e))?;

    let expected_len = path_len
        .parse::<usize>()
        .map_err(|e| format!("invalid path_len: {}", e))?;
    if path.as_bytes().len() != expected_len {
        return Err(format!(
            "path length mismatch: expected {} bytes, got {} bytes",
            expected_len,
            path.as_bytes().len()
        ));
    }

    if source_kind == BinarySourceKind::AssetBundle && asset_index.is_none() {
        return Err("asset_index is required for bundle keys".to_string());
    }

    Ok(BinaryObjectKey {
        source: BinarySource::Path(PathBuf::from(path)),
        source_kind,
        asset_index,
        path_id,
    })
}

fn parse_bok2(s: &str) -> std::result::Result<BinaryObjectKey, String> {
    let prefix = "bok2|";
    let mut rest = &s[prefix.len()..];

    let (kind, r) = split_once(rest, '|').ok_or_else(|| "missing kind".to_string())?;
    rest = r;
    let (asset_index, r) =
        split_once(rest, '|').ok_or_else(|| "missing asset_index".to_string())?;
    rest = r;
    let (path_id, r) = split_once(rest, '|').ok_or_else(|| "missing path_id".to_string())?;
    rest = r;
    let (outer_len, r) = split_once(rest, '|').ok_or_else(|| "missing outer_len".to_string())?;
    rest = r;

    let source_kind = parse_kind(kind)?;
    let asset_index = parse_asset_index(asset_index)?;
    let path_id = path_id
        .parse::<i64>()
        .map_err(|e| format!("invalid path_id: {}", e))?;

    let outer_len = outer_len
        .parse::<usize>()
        .map_err(|e| format!("invalid outer_len: {}", e))?;
    if rest.as_bytes().len() < outer_len {
        return Err("outer is shorter than outer_len".to_string());
    }

    let outer = rest
        .get(..outer_len)
        .ok_or_else(|| "outer_len splits UTF-8 boundary".to_string())?;
    let rest = rest
        .get(outer_len..)
        .ok_or_else(|| "outer_len splits UTF-8 boundary".to_string())?;

    let rest = rest
        .strip_prefix('|')
        .ok_or_else(|| "missing entry delimiter".to_string())?;
    let (entry_len, rest) = split_once(rest, '|').ok_or_else(|| "missing entry_len".to_string())?;
    let entry_len = entry_len
        .parse::<usize>()
        .map_err(|e| format!("invalid entry_len: {}", e))?;
    if rest.as_bytes().len() != entry_len {
        return Err(format!(
            "entry length mismatch: expected {} bytes, got {} bytes",
            entry_len,
            rest.as_bytes().len()
        ));
    }

    if source_kind == BinarySourceKind::AssetBundle && asset_index.is_none() {
        return Err("asset_index is required for bundle keys".to_string());
    }

    let source = if entry_len == 0 {
        BinarySource::Path(PathBuf::from(outer))
    } else {
        BinarySource::WebEntry {
            web_path: PathBuf::from(outer),
            entry_name: rest.to_string(),
        }
    };

    Ok(BinaryObjectKey {
        source,
        source_kind,
        asset_index,
        path_id,
    })
}

fn split_once<'a>(s: &'a str, delim: char) -> Option<(&'a str, &'a str)> {
    let pos = s.find(delim)?;
    Some((&s[..pos], &s[pos + delim.len_utf8()..]))
}
