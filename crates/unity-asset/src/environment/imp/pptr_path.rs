use super::{Result, UnityAssetError, UnityClass, UnityValue};

fn value_get_child<'a>(value: &'a UnityValue, key: &str) -> Option<&'a UnityValue> {
    match value {
        UnityValue::Object(map) => map.get(key),
        _ => None,
    }
}

fn value_get_child_mut<'a>(value: &'a mut UnityValue, key: &str) -> Option<&'a mut UnityValue> {
    match value {
        UnityValue::Object(map) => map.get_mut(key),
        _ => None,
    }
}

pub(crate) fn get_value_at_path<'a>(class: &'a UnityClass, path: &str) -> Option<&'a UnityValue> {
    let mut it = path.split('.').filter(|s| !s.is_empty());
    let first = it.next()?;
    let mut cur = class.get(first)?;
    for seg in it {
        cur = value_get_child(cur, seg)?;
    }
    Some(cur)
}

pub(crate) fn get_value_at_path_mut<'a>(
    class: &'a mut UnityClass,
    path: &str,
) -> Result<&'a mut UnityValue> {
    let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err(UnityAssetError::format("PPtr path is empty"));
    }

    let first = segments[0];
    if segments.len() == 1 {
        if class.get(first).is_none() {
            class.set(first.to_string(), UnityValue::Object(Default::default()));
        }
        return Ok(class
            .get_mut(first)
            .ok_or_else(|| UnityAssetError::format("Failed to access path root"))?);
    }

    let mut cur: &mut UnityValue = class.get_mut(first).ok_or_else(|| {
        UnityAssetError::format(format!("PPtr path missing required root field: {}", first))
    })?;

    for seg in &segments[1..segments.len() - 1] {
        cur = value_get_child_mut(cur, seg).ok_or_else(|| {
            UnityAssetError::format(format!("PPtr path missing required segment: {}", seg))
        })?;
    }

    let leaf = segments[segments.len() - 1];
    match cur {
        UnityValue::Object(map) => Ok(map
            .entry(leaf.to_string())
            .or_insert_with(|| UnityValue::Object(Default::default()))),
        _ => Err(UnityAssetError::format(format!(
            "PPtr path parent is not an object: {}",
            segments[segments.len() - 2]
        ))),
    }
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
