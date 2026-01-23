use super::object_graph::{EnvironmentObjectKey, YamlObjectKey};
use super::*;

#[derive(Debug, Clone, Copy)]
pub struct YamlPptrReferenceSearchOptions {
    pub max_objects: Option<usize>,
    pub max_results: Option<usize>,
    pub max_pptrs_per_object: Option<usize>,
}

impl Default for YamlPptrReferenceSearchOptions {
    fn default() -> Self {
        Self {
            max_objects: None,
            max_results: None,
            max_pptrs_per_object: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlPptrReference {
    pub from: YamlObjectKey,
    pub pptr_path: String,
    pub file_id: i64,
    pub guid: Option<[u8; 16]>,
    pub type_id: Option<i64>,
    pub asset_path: Option<PathBuf>,
    pub resolved: Option<EnvironmentObjectKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct YamlPptrRef {
    pub file_id: i64,
    pub guid: Option<[u8; 16]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct YamlPptrAtPath {
    path: String,
    file_id: i64,
    guid: Option<[u8; 16]>,
    type_id: Option<i64>,
}

pub(crate) fn parse_yaml_pptr(value: &unity_asset_core::UnityValue) -> Option<YamlPptrRef> {
    let map = value.as_object()?;
    if map.is_empty() || map.len() > 3 {
        return None;
    }
    for k in map.keys() {
        if k != "fileID" && k != "guid" && k != "type" {
            return None;
        }
    }

    let file_id = map.get("fileID")?.as_i64()?;
    if file_id == 0 {
        return None;
    }

    let guid = map
        .get("guid")
        .and_then(|v| v.as_str())
        .and_then(super::meta_guid::parse_guid_32_hex)
        .filter(|g| *g != [0u8; 16]);

    Some(YamlPptrRef { file_id, guid })
}

pub(crate) fn scan_yaml_pptrs(value: &UnityValue, out: &mut Vec<YamlPptrRef>) {
    if let Some(pptr) = parse_yaml_pptr(value) {
        out.push(pptr);
        return;
    }

    match value {
        UnityValue::Array(items) => {
            for v in items {
                scan_yaml_pptrs(v, out);
            }
        }
        UnityValue::Object(map) => {
            for v in map.values() {
                scan_yaml_pptrs(v, out);
            }
        }
        _ => {}
    }
}

fn scan_yaml_pptrs_with_paths(class: &UnityClass, max_pptrs: Option<usize>) -> Vec<YamlPptrAtPath> {
    fn parse_at_path(value: &UnityValue, path: &str) -> Option<YamlPptrAtPath> {
        let map = value.as_object()?;
        if map.is_empty() || map.len() > 3 {
            return None;
        }
        for k in map.keys() {
            if k != "fileID" && k != "guid" && k != "type" {
                return None;
            }
        }

        let file_id = map.get("fileID")?.as_i64()?;
        if file_id == 0 {
            return None;
        }

        let guid = map
            .get("guid")
            .and_then(|v| v.as_str())
            .and_then(super::meta_guid::parse_guid_32_hex)
            .filter(|g| *g != [0u8; 16]);

        let type_id = map.get("type").and_then(|v| v.as_i64());

        Some(YamlPptrAtPath {
            path: path.to_string(),
            file_id,
            guid,
            type_id,
        })
    }

    fn scan_value(
        value: &UnityValue,
        prefix: &str,
        out: &mut Vec<YamlPptrAtPath>,
        max: Option<usize>,
    ) {
        if let Some(max) = max
            && out.len() >= max
        {
            return;
        }

        if let Some(pptr) = parse_at_path(value, prefix) {
            out.push(pptr);
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

    let mut out: Vec<YamlPptrAtPath> = Vec::new();
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

impl Environment {
    pub fn find_yaml_pptr_references_to(
        &self,
        target: &EnvironmentObjectKey,
        options: YamlPptrReferenceSearchOptions,
    ) -> Result<Vec<YamlPptrReference>> {
        use std::collections::HashMap;

        let mut anchor_index: HashMap<PathBuf, HashMap<String, YamlObjectKey>> = HashMap::new();
        for (path, doc) in &self.yaml_documents {
            let mut map: HashMap<String, YamlObjectKey> = HashMap::new();
            for obj in doc.entries() {
                let key = YamlObjectKey {
                    path: path.clone(),
                    anchor: obj.anchor.clone(),
                };
                map.insert(obj.anchor.clone(), key);
            }
            anchor_index.insert(path.clone(), map);
        }

        let mut out: Vec<YamlPptrReference> = Vec::new();
        let mut scanned_objects = 0usize;

        for (path, doc) in &self.yaml_documents {
            if let Some(max) = options.max_results
                && out.len() >= max
            {
                break;
            }

            let anchors = anchor_index.get(path);
            for obj in doc.entries() {
                if let Some(max) = options.max_objects
                    && scanned_objects >= max
                {
                    break;
                }
                if let Some(max) = options.max_results
                    && out.len() >= max
                {
                    break;
                }

                scanned_objects = scanned_objects.saturating_add(1);

                let from = YamlObjectKey {
                    path: path.clone(),
                    anchor: obj.anchor.clone(),
                };

                let pptrs = scan_yaml_pptrs_with_paths(obj, options.max_pptrs_per_object);
                for pptr in pptrs {
                    if let Some(max) = options.max_results
                        && out.len() >= max
                    {
                        break;
                    }

                    let file_id_str = pptr.file_id.to_string();
                    let (asset_path, resolved) = if let Some(guid) = pptr.guid {
                        let asset_path = self.asset_path_for_guid(guid);

                        let mut resolved: Option<EnvironmentObjectKey> = None;
                        if let Some(asset_source_path) = &asset_path {
                            if let Some(targets) = anchor_index.get(asset_source_path) {
                                if let Some(target) = targets.get(&file_id_str) {
                                    resolved = Some(EnvironmentObjectKey::Yaml(target.clone()));
                                }
                            }
                            if resolved.is_none() {
                                if let Some(obj_ref) = self
                                    .find_binary_object_in_source(asset_source_path, pptr.file_id)
                                {
                                    resolved = Some(EnvironmentObjectKey::Binary(obj_ref.key()));
                                }
                            }
                        }

                        (asset_path, resolved)
                    } else {
                        let mut resolved: Option<EnvironmentObjectKey> = None;
                        if let Some(anchors) = anchors {
                            if let Some(target) = anchors.get(&file_id_str) {
                                resolved = Some(EnvironmentObjectKey::Yaml(target.clone()));
                            }
                        }
                        (None, resolved)
                    };

                    let matches = if let Some(resolved) = &resolved {
                        resolved == target
                    } else {
                        match target {
                            EnvironmentObjectKey::Yaml(t) => {
                                pptr.guid.is_none()
                                    && &from.path == &t.path
                                    && pptr.file_id.to_string() == t.anchor
                            }
                            EnvironmentObjectKey::Binary(_) => false,
                        }
                    };

                    if !matches {
                        continue;
                    }

                    out.push(YamlPptrReference {
                        from: from.clone(),
                        pptr_path: pptr.path,
                        file_id: pptr.file_id,
                        guid: pptr.guid,
                        type_id: pptr.type_id,
                        asset_path,
                        resolved,
                    });
                }
            }
        }

        out.sort_by(|a, b| {
            a.from
                .path
                .to_string_lossy()
                .cmp(&b.from.path.to_string_lossy())
                .then_with(|| a.from.anchor.cmp(&b.from.anchor))
                .then_with(|| a.pptr_path.cmp(&b.pptr_path))
        });
        out.dedup();
        Ok(out)
    }
}
