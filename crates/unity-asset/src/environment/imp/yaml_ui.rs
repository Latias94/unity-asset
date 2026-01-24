use super::object_graph::YamlObjectKey;
use super::path::canonicalize_if_exists;
use super::*;

fn vec2_value(x: f64, y: f64) -> UnityValue {
    UnityValue::Object(
        [
            ("x".to_string(), UnityValue::Float(x)),
            ("y".to_string(), UnityValue::Float(y)),
        ]
        .into_iter()
        .collect(),
    )
}

fn read_gameobject_component_file_ids(game_object: &UnityClass) -> Vec<i64> {
    let Some(value) = super::pptr_path::get_value_at_path(game_object, "m_Component") else {
        return Vec::new();
    };
    let UnityValue::Array(items) = value else {
        return Vec::new();
    };

    let mut out: Vec<i64> = Vec::new();
    for item in items {
        let UnityValue::Object(map) = item else {
            continue;
        };
        let Some(component) = map.get("component") else {
            continue;
        };
        let Some(pptr) = super::yaml_pptr::parse_yaml_pptr(component) else {
            continue;
        };
        if pptr.guid.is_some() {
            continue;
        }
        out.push(pptr.file_id);
    }
    out
}

impl<'a> EnvironmentEditSession<'a> {
    pub fn set_yaml_value_at_key_path(
        &mut self,
        key: &YamlObjectKey,
        field_path: &str,
        value: UnityValue,
    ) -> Result<()> {
        self.set_yaml_value_at_path(&key.path, key.anchor.as_str(), field_path, value)
    }

    pub fn set_yaml_string_at_key_path(
        &mut self,
        key: &YamlObjectKey,
        field_path: &str,
        value: &str,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(key, field_path, UnityValue::String(value.to_string()))
    }

    pub fn set_yaml_vec2_at_key_path(
        &mut self,
        key: &YamlObjectKey,
        field_path: &str,
        x: f64,
        y: f64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(key, field_path, vec2_value(x, y))
    }

    pub fn find_yaml_gameobject_key_by_name(
        &mut self,
        yaml_path: &Path,
        name: &str,
    ) -> Result<YamlObjectKey> {
        self.find_yaml_object_key_in_file_by_field_string_unique(
            yaml_path,
            Some("GameObject"),
            "m_Name",
            name,
        )
    }

    pub fn find_yaml_component_key_by_class_name(
        &mut self,
        game_object: &YamlObjectKey,
        component_class_name: &str,
    ) -> Result<YamlObjectKey> {
        let yaml_path = canonicalize_if_exists(&game_object.path);
        let yaml_key = self.env_mut().ensure_yaml_loaded(&yaml_path)?;

        let doc = self
            .env()
            .yaml_documents
            .get(&yaml_key)
            .expect("ensure_yaml_loaded inserts yaml_documents");
        let go = doc
            .entries()
            .iter()
            .find(|o| o.anchor == game_object.anchor)
            .ok_or_else(|| {
                UnityAssetError::format(format!(
                    "GameObject anchor not found: {} (file: {})",
                    game_object.anchor,
                    yaml_key.display()
                ))
            })?;

        for file_id in read_gameobject_component_file_ids(go) {
            let anchor = file_id.to_string();
            let Some(component) = doc.entries().iter().find(|o| o.anchor == anchor) else {
                continue;
            };
            if component.class_name == component_class_name {
                return Ok(YamlObjectKey {
                    path: yaml_key.clone(),
                    anchor,
                });
            }
        }

        Err(UnityAssetError::format(format!(
            "Component not found on GameObject {}: {} (file: {})",
            game_object.anchor,
            component_class_name,
            yaml_key.display()
        )))
    }

    pub fn find_yaml_monobehaviour_key_by_script_guid(
        &mut self,
        game_object: &YamlObjectKey,
        script_guid_32_hex: &str,
    ) -> Result<YamlObjectKey> {
        let script_guid_32_hex = script_guid_32_hex.trim().to_ascii_lowercase();
        let yaml_path = canonicalize_if_exists(&game_object.path);
        let yaml_key = self.env_mut().ensure_yaml_loaded(&yaml_path)?;

        let doc = self
            .env()
            .yaml_documents
            .get(&yaml_key)
            .expect("ensure_yaml_loaded inserts yaml_documents");
        let go = doc
            .entries()
            .iter()
            .find(|o| o.anchor == game_object.anchor)
            .ok_or_else(|| {
                UnityAssetError::format(format!(
                    "GameObject anchor not found: {} (file: {})",
                    game_object.anchor,
                    yaml_key.display()
                ))
            })?;

        for file_id in read_gameobject_component_file_ids(go) {
            let anchor = file_id.to_string();
            let Some(component) = doc.entries().iter().find(|o| o.anchor == anchor) else {
                continue;
            };
            if component.class_name != "MonoBehaviour" {
                continue;
            }
            let Some(guid_value) = super::pptr_path::get_value_at_path(component, "m_Script.guid")
            else {
                continue;
            };
            let Some(guid_str) = guid_value.as_str() else {
                continue;
            };
            if guid_str.trim().to_ascii_lowercase() == script_guid_32_hex {
                return Ok(YamlObjectKey {
                    path: yaml_key.clone(),
                    anchor,
                });
            }
        }

        Err(UnityAssetError::format(format!(
            "MonoBehaviour not found on GameObject {} with m_Script.guid == {} (file: {})",
            game_object.anchor,
            script_guid_32_hex,
            yaml_key.display()
        )))
    }

    pub fn yaml_rect_transform_set_anchored_position(
        &mut self,
        rect_transform: &YamlObjectKey,
        x: f64,
        y: f64,
    ) -> Result<()> {
        self.set_yaml_vec2_at_key_path(rect_transform, "m_AnchoredPosition", x, y)
    }

    pub fn yaml_rect_transform_set_size_delta(
        &mut self,
        rect_transform: &YamlObjectKey,
        x: f64,
        y: f64,
    ) -> Result<()> {
        self.set_yaml_vec2_at_key_path(rect_transform, "m_SizeDelta", x, y)
    }
}
