use super::{Result, UnityAssetError, UnityClass, UnityValue};

#[derive(Debug, Clone)]
struct PathSegment {
    name: String,
    index: Option<usize>,
}

fn parse_path(path: &str) -> Result<Vec<PathSegment>> {
    let mut out = Vec::new();
    for raw in path.split('.').filter(|s| !s.is_empty()) {
        out.push(parse_segment(raw)?);
    }
    if out.is_empty() {
        return Err(UnityAssetError::format("PPtr path is empty"));
    }
    Ok(out)
}

fn parse_segment(seg: &str) -> Result<PathSegment> {
    let Some(bracket) = seg.find('[') else {
        return Ok(PathSegment {
            name: seg.to_string(),
            index: None,
        });
    };

    if !seg.ends_with(']') {
        return Err(UnityAssetError::format(format!(
            "Invalid PPtr path segment (missing ']'): {}",
            seg
        )));
    }

    let name = &seg[..bracket];
    let idx_str = &seg[bracket + 1..seg.len() - 1];
    if name.is_empty() {
        return Err(UnityAssetError::format(format!(
            "Invalid PPtr path segment (empty name): {}",
            seg
        )));
    }

    let index: usize = idx_str.parse().map_err(|_| {
        UnityAssetError::format(format!(
            "Invalid PPtr path segment index '{}': {}",
            idx_str, seg
        ))
    })?;

    Ok(PathSegment {
        name: name.to_string(),
        index: Some(index),
    })
}

fn value_get_child<'a>(value: &'a UnityValue, key: &str) -> Option<&'a UnityValue> {
    match value {
        UnityValue::Object(map) => map.get(key),
        _ => None,
    }
}

fn array_get(value: &UnityValue, idx: usize) -> Option<&UnityValue> {
    match value {
        UnityValue::Array(v) => v.get(idx),
        _ => None,
    }
}

pub(crate) fn get_value_at_path<'a>(class: &'a UnityClass, path: &str) -> Option<&'a UnityValue> {
    let segs = parse_path(path).ok()?;
    let first = segs.first()?;

    let mut cur = class.get(first.name.as_str())?;
    if let Some(idx) = first.index {
        cur = array_get(cur, idx)?;
    }

    for seg in &segs[1..] {
        cur = value_get_child(cur, seg.name.as_str())?;
        if let Some(idx) = seg.index {
            cur = array_get(cur, idx)?;
        }
    }

    Some(cur)
}

pub(crate) fn read_pptr(value: &UnityValue) -> Option<(i32, i64)> {
    let UnityValue::Object(map) = value else {
        return None;
    };

    let fields = pptr_field_indices(map).ok()?;
    let file_id = fields
        .file_id
        .and_then(|index| map.get_index(index))
        .map(|(_, value)| value)
        .and_then(|v| v.as_i64())
        .and_then(|v| i32::try_from(v).ok())?;
    let path_id = fields
        .path_id
        .and_then(|index| map.get_index(index))
        .map(|(_, value)| value)
        .and_then(|v| v.as_i64())?;
    Some((file_id, path_id))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PPtrFieldIndices {
    file_id: Option<usize>,
    path_id: Option<usize>,
}

fn pptr_field_indices(map: &indexmap::IndexMap<String, UnityValue>) -> Result<PPtrFieldIndices> {
    let mut fields = PPtrFieldIndices::default();
    for (index, name) in map.keys().enumerate() {
        let slot = if is_file_id_name(name) {
            &mut fields.file_id
        } else if is_path_id_name(name) {
            &mut fields.path_id
        } else {
            continue;
        };
        if slot.replace(index).is_some() {
            let role = if is_file_id_name(name) {
                "file ID"
            } else {
                "path ID"
            };
            return Err(UnityAssetError::format(format!(
                "PPtr object has duplicate {role} fields"
            )));
        }
    }
    Ok(fields)
}

fn is_file_id_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("fileID") || name.eq_ignore_ascii_case("m_FileID")
}

fn is_path_id_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("pathID") || name.eq_ignore_ascii_case("m_PathID")
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::*;

    #[test]
    fn read_pptr_accepts_canonical_and_alias_field_names() {
        let canonical = UnityValue::Object(IndexMap::from([
            ("m_FileID".to_owned(), UnityValue::Integer(0)),
            ("m_PathID".to_owned(), UnityValue::Integer(99)),
        ]));
        let aliases = UnityValue::Object(IndexMap::from([
            ("fileID".to_owned(), UnityValue::Integer(3)),
            ("m_Tag".to_owned(), UnityValue::Integer(7)),
            ("pathID".to_owned(), UnityValue::Integer(101)),
        ]));

        assert_eq!(read_pptr(&canonical), Some((0, 99)));
        assert_eq!(read_pptr(&aliases), Some((3, 101)));
    }

    #[test]
    fn read_pptr_rejects_duplicates_partial_roles_and_wrong_value_types() {
        let duplicate = UnityValue::Object(IndexMap::from([
            ("fileID".to_owned(), UnityValue::Integer(1)),
            ("m_FileID".to_owned(), UnityValue::Integer(2)),
            ("pathID".to_owned(), UnityValue::Integer(3)),
        ]));
        assert_eq!(read_pptr(&duplicate), None);

        let partial = UnityValue::Object(IndexMap::from([(
            "m_FileID".to_owned(),
            UnityValue::Integer(1),
        )]));
        assert_eq!(read_pptr(&partial), None);

        let wrong_value_types = UnityValue::Object(IndexMap::from([
            (
                "m_FileID".to_owned(),
                UnityValue::String("not-an-integer".to_owned()),
            ),
            ("m_PathID".to_owned(), UnityValue::Bool(false)),
        ]));
        assert_eq!(read_pptr(&wrong_value_types), None);
    }
}
