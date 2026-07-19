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

fn empty_value_for_segment(seg: &PathSegment) -> UnityValue {
    if seg.index.is_some() {
        UnityValue::Array(Vec::new())
    } else {
        UnityValue::Object(Default::default())
    }
}

fn array_ensure_index(value: &mut UnityValue, idx: usize) -> &mut UnityValue {
    if !matches!(value, UnityValue::Array(_)) {
        *value = UnityValue::Array(Vec::new());
    }
    let UnityValue::Array(v) = value else {
        unreachable!();
    };
    if v.len() <= idx {
        v.resize(idx + 1, UnityValue::Null);
    }
    &mut v[idx]
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

pub(crate) fn get_value_at_path_mut<'a>(
    class: &'a mut UnityClass,
    path: &str,
) -> Result<&'a mut UnityValue> {
    let segs = parse_path(path)?;
    let first = &segs[0];

    if class.get(&first.name).is_none() {
        class.set(first.name.clone(), empty_value_for_segment(first));
    }

    let mut cur: &mut UnityValue = class.get_mut(&first.name).ok_or_else(|| {
        UnityAssetError::format(format!(
            "PPtr path missing required root field: {}",
            first.name
        ))
    })?;
    if let Some(idx) = first.index {
        cur = array_ensure_index(cur, idx);
    }

    for seg in &segs[1..] {
        cur = match cur {
            UnityValue::Object(map) => map
                .entry(seg.name.clone())
                .or_insert_with(|| empty_value_for_segment(seg)),
            _ => {
                return Err(UnityAssetError::format(format!(
                    "PPtr path parent is not an object: {}",
                    seg.name
                )));
            }
        };

        if let Some(idx) = seg.index {
            cur = array_ensure_index(cur, idx);
        }
    }

    Ok(cur)
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

pub(crate) fn write_pptr(value: &mut UnityValue, file_id: i32, path_id: i64) -> Result<()> {
    if !matches!(value, UnityValue::Object(_)) {
        *value = UnityValue::Object(Default::default());
    }
    let UnityValue::Object(map) = value else {
        return Err(UnityAssetError::format(
            "PPtr value could not be converted to an object",
        ));
    };

    let fields = pptr_field_indices(map)?;
    let file_id_value = UnityValue::Integer(i64::from(file_id));
    let path_id_value = UnityValue::Integer(path_id);

    match (fields.file_id, fields.path_id) {
        (Some(file_index), Some(path_index)) => {
            let (_, field) = map.get_index_mut(file_index).ok_or_else(|| {
                UnityAssetError::format("PPtr file ID field index became invalid")
            })?;
            *field = file_id_value;

            let (_, field) = map.get_index_mut(path_index).ok_or_else(|| {
                UnityAssetError::format("PPtr path ID field index became invalid")
            })?;
            *field = path_id_value;
        }
        (None, None) => {
            map.insert("m_FileID".to_owned(), file_id_value);
            map.insert("m_PathID".to_owned(), path_id_value);
        }
        (Some(_), None) => {
            return Err(UnityAssetError::format(
                "PPtr object has a file ID field but no path ID field",
            ));
        }
        (None, Some(_)) => {
            return Err(UnityAssetError::format(
                "PPtr object has a path ID field but no file ID field",
            ));
        }
    }

    Ok(())
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

pub(crate) fn write_pptr_at_path(
    class: &mut UnityClass,
    path: &str,
    file_id: i32,
    path_id: i64,
) -> Result<()> {
    let v = get_value_at_path_mut(class, path)?;
    write_pptr(v, file_id, path_id)
}

pub(crate) fn set_value_at_path(
    class: &mut UnityClass,
    path: &str,
    value: UnityValue,
) -> Result<()> {
    let v = get_value_at_path_mut(class, path)?;
    *v = value;
    Ok(())
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::*;

    #[test]
    fn write_pptr_updates_the_existing_schema_names_without_injecting_aliases() {
        let mut value = UnityValue::Object(IndexMap::from([
            ("m_FileID".to_owned(), UnityValue::Integer(0)),
            ("m_PathID".to_owned(), UnityValue::Integer(1)),
        ]));

        write_pptr(&mut value, 2, 99).unwrap();

        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 2);
        assert_eq!(object.get("m_FileID").and_then(UnityValue::as_i64), Some(2));
        assert_eq!(
            object.get("m_PathID").and_then(UnityValue::as_i64),
            Some(99)
        );
        assert!(!object.contains_key("fileID"));
        assert!(!object.contains_key("pathID"));
    }

    #[test]
    fn write_pptr_preserves_extension_fields_and_alias_spelling() {
        let mut value = UnityValue::Object(IndexMap::from([
            ("fileID".to_owned(), UnityValue::Integer(0)),
            ("m_Tag".to_owned(), UnityValue::Integer(7)),
            ("pathID".to_owned(), UnityValue::Integer(1)),
        ]));

        write_pptr(&mut value, 3, 101).unwrap();

        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 3);
        assert_eq!(object.get("fileID").and_then(UnityValue::as_i64), Some(3));
        assert_eq!(object.get("pathID").and_then(UnityValue::as_i64), Some(101));
        assert_eq!(object.get("m_Tag").and_then(UnityValue::as_i64), Some(7));
        assert!(!object.contains_key("m_FileID"));
        assert!(!object.contains_key("m_PathID"));
    }

    #[test]
    fn pptr_shape_rejects_duplicates_and_partial_roles() {
        let duplicate = UnityValue::Object(IndexMap::from([
            ("fileID".to_owned(), UnityValue::Integer(1)),
            ("m_FileID".to_owned(), UnityValue::Integer(2)),
            ("pathID".to_owned(), UnityValue::Integer(3)),
        ]));
        assert_eq!(read_pptr(&duplicate), None);
        let mut duplicate_for_write = duplicate.clone();
        assert!(write_pptr(&mut duplicate_for_write, 4, 5).is_err());
        assert_eq!(duplicate_for_write, duplicate);

        let partial = UnityValue::Object(IndexMap::from([(
            "m_FileID".to_owned(),
            UnityValue::Integer(1),
        )]));
        assert_eq!(read_pptr(&partial), None);
        let mut partial_for_write = partial.clone();
        assert!(write_pptr(&mut partial_for_write, 4, 5).is_err());
        assert_eq!(partial_for_write, partial);

        let mut wrong_value_types = UnityValue::Object(IndexMap::from([
            (
                "m_FileID".to_owned(),
                UnityValue::String("not-an-integer".to_owned()),
            ),
            ("m_PathID".to_owned(), UnityValue::Bool(false)),
        ]));
        assert_eq!(read_pptr(&wrong_value_types), None);
        write_pptr(&mut wrong_value_types, 6, 7).unwrap();
        assert_eq!(read_pptr(&wrong_value_types), Some((6, 7)));
    }

    #[test]
    fn write_pptr_synthesizes_the_binary_canonical_shape_for_empty_values() {
        let mut value = UnityValue::Null;

        write_pptr(&mut value, 0, 42).unwrap();

        assert_eq!(read_pptr(&value), Some((0, 42)));
        let object = value.as_object().unwrap();
        assert_eq!(
            object.keys().map(String::as_str).collect::<Vec<_>>(),
            ["m_FileID", "m_PathID"]
        );
    }
}
