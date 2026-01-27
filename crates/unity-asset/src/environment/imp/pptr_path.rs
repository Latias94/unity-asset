use super::{Result, UnityAssetError, UnityClass, UnityValue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PptrAtPath {
    pub path: String,
    pub file_id: i32,
    pub path_id: i64,
}

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

    let file_id = map
        .get("fileID")
        .or_else(|| map.get("m_FileID"))
        .and_then(|v| v.as_i64())
        .and_then(|v| i32::try_from(v).ok())?;
    let path_id = map
        .get("pathID")
        .or_else(|| map.get("m_PathID"))
        .and_then(|v| v.as_i64())?;
    Some((file_id, path_id))
}

pub(crate) fn scan_pptrs_with_paths(
    class: &UnityClass,
    max_pptrs: Option<usize>,
) -> Vec<PptrAtPath> {
    fn scan_value(value: &UnityValue, prefix: &str, out: &mut Vec<PptrAtPath>, max: Option<usize>) {
        if let Some(max) = max
            && out.len() >= max
        {
            return;
        }

        if let Some((file_id, path_id)) = read_pptr(value) {
            out.push(PptrAtPath {
                path: prefix.to_string(),
                file_id,
                path_id,
            });
            return;
        }

        match value {
            UnityValue::Object(map) => {
                for (key, child) in map {
                    let next = if prefix.is_empty() {
                        key.to_string()
                    } else {
                        format!("{}.{}", prefix, key)
                    };
                    scan_value(child, &next, out, max);
                }
            }
            UnityValue::Array(arr) => {
                for (idx, child) in arr.iter().enumerate() {
                    let next = if prefix.is_empty() {
                        format!("[{}]", idx)
                    } else {
                        format!("{}[{}]", prefix, idx)
                    };
                    scan_value(child, &next, out, max);
                }
            }
            _ => {}
        }
    }

    let mut out: Vec<PptrAtPath> = Vec::new();
    for (key, value) in class.properties() {
        scan_value(value, key, &mut out, max_pptrs);
        if let Some(max) = max_pptrs
            && out.len() >= max
        {
            break;
        }
    }
    out
}

pub(crate) fn write_pptr(value: &mut UnityValue, file_id: i32, path_id: i64) {
    if !matches!(value, UnityValue::Object(_)) {
        *value = UnityValue::Object(Default::default());
    }
    let UnityValue::Object(map) = value else {
        return;
    };

    let file_id_value = UnityValue::Integer(file_id as i64);
    let path_id_value = UnityValue::Integer(path_id);

    for key in ["fileID", "m_FileID"] {
        map.insert(key.to_string(), file_id_value.clone());
    }
    for key in ["pathID", "m_PathID"] {
        map.insert(key.to_string(), path_id_value.clone());
    }
}

pub(crate) fn write_pptr_at_path(
    class: &mut UnityClass,
    path: &str,
    file_id: i32,
    path_id: i64,
) -> Result<()> {
    let v = get_value_at_path_mut(class, path)?;
    write_pptr(v, file_id, path_id);
    Ok(())
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
    use super::*;

    #[test]
    fn scan_pptrs_with_paths_emits_dot_and_index_paths() {
        let mut class = UnityClass::new(0, "Test".to_string(), "0".to_string());
        class.set(
            "root".to_string(),
            UnityValue::Object(
                [
                    (
                        "m_Ptr".to_string(),
                        UnityValue::Object(
                            [
                                ("fileID".to_string(), UnityValue::Integer(1)),
                                ("pathID".to_string(), UnityValue::Integer(2)),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    ),
                    (
                        "arr".to_string(),
                        UnityValue::Array(vec![UnityValue::Object(
                            [
                                ("m_FileID".to_string(), UnityValue::Integer(0)),
                                ("m_PathID".to_string(), UnityValue::Integer(42)),
                            ]
                            .into_iter()
                            .collect(),
                        )]),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
        );

        let pptrs = scan_pptrs_with_paths(&class, None);
        assert_eq!(
            pptrs,
            vec![
                PptrAtPath {
                    path: "root.m_Ptr".to_string(),
                    file_id: 1,
                    path_id: 2,
                },
                PptrAtPath {
                    path: "root.arr[0]".to_string(),
                    file_id: 0,
                    path_id: 42,
                },
            ]
        );
    }
}
