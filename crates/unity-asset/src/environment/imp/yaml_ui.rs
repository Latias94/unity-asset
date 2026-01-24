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

fn vec3_value(x: f64, y: f64, z: f64) -> UnityValue {
    UnityValue::Object(
        [
            ("x".to_string(), UnityValue::Float(x)),
            ("y".to_string(), UnityValue::Float(y)),
            ("z".to_string(), UnityValue::Float(z)),
        ]
        .into_iter()
        .collect(),
    )
}

fn quat_value(x: f64, y: f64, z: f64, w: f64) -> UnityValue {
    UnityValue::Object(
        [
            ("x".to_string(), UnityValue::Float(x)),
            ("y".to_string(), UnityValue::Float(y)),
            ("z".to_string(), UnityValue::Float(z)),
            ("w".to_string(), UnityValue::Float(w)),
        ]
        .into_iter()
        .collect(),
    )
}

fn color_rgba_value(r: f64, g: f64, b: f64, a: f64) -> UnityValue {
    UnityValue::Object(
        [
            ("r".to_string(), UnityValue::Float(r)),
            ("g".to_string(), UnityValue::Float(g)),
            ("b".to_string(), UnityValue::Float(b)),
            ("a".to_string(), UnityValue::Float(a)),
        ]
        .into_iter()
        .collect(),
    )
}

fn yaml_pptr_value(file_id: i64, guid_32_hex: Option<&str>, type_id: Option<i64>) -> UnityValue {
    let mut entries: Vec<(String, UnityValue)> = Vec::new();
    entries.push(("fileID".to_string(), UnityValue::Integer(file_id)));
    if let Some(guid) = guid_32_hex {
        entries.push(("guid".to_string(), UnityValue::String(guid.to_string())));
    }
    if let Some(type_id) = type_id {
        entries.push(("type".to_string(), UnityValue::Integer(type_id)));
    }
    UnityValue::Object(entries.into_iter().collect())
}

fn ensure_array_mut(value: &mut UnityValue) -> &mut Vec<UnityValue> {
    if !matches!(value, UnityValue::Array(_)) {
        *value = UnityValue::Array(Vec::new());
    }
    match value {
        UnityValue::Array(v) => v,
        _ => unreachable!(),
    }
}

fn unity_event_default_args() -> UnityValue {
    UnityValue::Object(
        [
            (
                "m_ObjectArgument".to_string(),
                yaml_pptr_value(0, None, None),
            ),
            (
                "m_ObjectArgumentAssemblyTypeName".to_string(),
                UnityValue::String(String::new()),
            ),
            ("m_IntArgument".to_string(), UnityValue::Integer(0)),
            ("m_FloatArgument".to_string(), UnityValue::Float(0.0)),
            (
                "m_StringArgument".to_string(),
                UnityValue::String(String::new()),
            ),
            ("m_BoolArgument".to_string(), UnityValue::Integer(0)),
        ]
        .into_iter()
        .collect(),
    )
}

fn unity_event_persistent_call(target: UnityValue, method_name: &str, mode: i64) -> UnityValue {
    UnityValue::Object(
        [
            ("m_Target".to_string(), target),
            (
                "m_MethodName".to_string(),
                UnityValue::String(method_name.to_string()),
            ),
            ("m_Mode".to_string(), UnityValue::Integer(mode)),
            ("m_Arguments".to_string(), unity_event_default_args()),
            ("m_CallState".to_string(), UnityValue::Integer(2)),
        ]
        .into_iter()
        .collect(),
    )
}

fn read_child_transform_file_ids(transform: &UnityClass) -> Vec<i64> {
    let Some(value) = super::pptr_path::get_value_at_path(transform, "m_Children") else {
        return Vec::new();
    };
    let UnityValue::Array(items) = value else {
        return Vec::new();
    };

    let mut out: Vec<i64> = Vec::new();
    for item in items {
        let Some(pptr) = super::yaml_pptr::parse_yaml_pptr(item) else {
            continue;
        };
        if pptr.guid.is_some() {
            continue;
        }
        out.push(pptr.file_id);
    }
    out
}

fn read_transform_gameobject_file_id(transform: &UnityClass) -> Option<i64> {
    let value = super::pptr_path::get_value_at_path(transform, "m_GameObject")?;
    let pptr = super::yaml_pptr::parse_yaml_pptr(value)?;
    if pptr.guid.is_some() {
        return None;
    }
    Some(pptr.file_id)
}

fn read_transform_parent_file_id(transform: &UnityClass) -> Option<i64> {
    let value = super::pptr_path::get_value_at_path(transform, "m_Father")?;
    let pptr = super::yaml_pptr::parse_yaml_pptr(value)?;
    if pptr.guid.is_some() {
        return None;
    }
    Some(pptr.file_id)
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
    fn yaml_doc_for_read(&mut self, yaml_path: &Path) -> Result<(PathBuf, &YamlDocument)> {
        let yaml_path = canonicalize_if_exists(yaml_path);
        let yaml_key = self.env_mut().ensure_yaml_loaded(&yaml_path)?;
        let env = self.env();
        let doc = env
            .write_state
            .yaml_documents
            .get(&yaml_key)
            .or_else(|| env.yaml_documents.get(&yaml_key))
            .expect("ensure_yaml_loaded ensures base yaml doc exists");
        Ok((yaml_key, doc))
    }

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

    pub fn set_yaml_value_at_key_path_first_match(
        &mut self,
        key: &YamlObjectKey,
        candidate_paths: &[&str],
        value: UnityValue,
    ) -> Result<()> {
        if candidate_paths.is_empty() {
            return Err(UnityAssetError::format("No candidate paths provided"));
        }

        self.env_mut()
            .edit_yaml_object_anchor(&key.path, key.anchor.as_str(), |class| {
                let mut chosen = candidate_paths[0];
                for p in candidate_paths {
                    if super::pptr_path::get_value_at_path(class, p).is_some() {
                        chosen = p;
                        break;
                    }
                }
                super::pptr_path::set_value_at_path(class, chosen, value)
            })
    }

    pub fn set_yaml_string_at_key_path_first_match(
        &mut self,
        key: &YamlObjectKey,
        candidate_paths: &[&str],
        value: &str,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path_first_match(
            key,
            candidate_paths,
            UnityValue::String(value.to_string()),
        )
    }

    pub fn set_yaml_vec3_at_key_path(
        &mut self,
        key: &YamlObjectKey,
        field_path: &str,
        x: f64,
        y: f64,
        z: f64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(key, field_path, vec3_value(x, y, z))
    }

    pub fn set_yaml_quat_at_key_path(
        &mut self,
        key: &YamlObjectKey,
        field_path: &str,
        x: f64,
        y: f64,
        z: f64,
        w: f64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(key, field_path, quat_value(x, y, z, w))
    }

    pub fn set_yaml_color_rgba_at_key_path(
        &mut self,
        key: &YamlObjectKey,
        field_path: &str,
        r: f64,
        g: f64,
        b: f64,
        a: f64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(key, field_path, color_rgba_value(r, g, b, a))
    }

    pub fn set_yaml_pptr_at_key_path(
        &mut self,
        key: &YamlObjectKey,
        field_path: &str,
        file_id: i64,
        guid_32_hex: Option<&str>,
        type_id: Option<i64>,
    ) -> Result<()> {
        let guid_32_hex = guid_32_hex.map(|s| s.trim().to_ascii_lowercase());
        self.set_yaml_value_at_key_path(
            key,
            field_path,
            yaml_pptr_value(file_id, guid_32_hex.as_deref(), type_id),
        )
    }

    pub fn set_yaml_pptr_at_key_path_first_match(
        &mut self,
        key: &YamlObjectKey,
        candidate_paths: &[&str],
        file_id: i64,
        guid_32_hex: Option<&str>,
        type_id: Option<i64>,
    ) -> Result<()> {
        let guid_32_hex = guid_32_hex.map(|s| s.trim().to_ascii_lowercase());
        self.set_yaml_value_at_key_path_first_match(
            key,
            candidate_paths,
            yaml_pptr_value(file_id, guid_32_hex.as_deref(), type_id),
        )
    }

    pub fn set_yaml_pptr_to_yaml_anchor_at_key_path(
        &mut self,
        key: &YamlObjectKey,
        field_path: &str,
        anchor: &str,
    ) -> Result<()> {
        let file_id = anchor.trim().parse::<i64>().map_err(|e| {
            UnityAssetError::format(format!(
                "Invalid YAML anchor fileID for PPtr: {:?} ({})",
                anchor, e
            ))
        })?;
        self.set_yaml_pptr_at_key_path(key, field_path, file_id, None, None)
    }

    pub fn yaml_gameobject_set_active(
        &mut self,
        game_object: &YamlObjectKey,
        active: bool,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            game_object,
            "m_IsActive",
            UnityValue::Integer(if active { 1 } else { 0 }),
        )
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

    pub fn find_yaml_transform_key_for_gameobject(
        &mut self,
        game_object: &YamlObjectKey,
    ) -> Result<YamlObjectKey> {
        match self.find_yaml_component_key_by_class_name(game_object, "RectTransform") {
            Ok(v) => Ok(v),
            Err(_) => self.find_yaml_component_key_by_class_name(game_object, "Transform"),
        }
    }

    pub fn find_yaml_child_gameobject_key_by_hierarchy_path(
        &mut self,
        root_game_object: &YamlObjectKey,
        hierarchy_path: &str,
    ) -> Result<YamlObjectKey> {
        let segments: Vec<&str> = hierarchy_path
            .split('/')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        if segments.is_empty() {
            return Ok(root_game_object.clone());
        }

        let yaml_path = canonicalize_if_exists(&root_game_object.path);
        let yaml_key = self.env_mut().ensure_yaml_loaded(&yaml_path)?;
        let doc = {
            let env = self.env();
            env.write_state
                .yaml_documents
                .get(&yaml_key)
                .or_else(|| env.yaml_documents.get(&yaml_key))
                .expect("ensure_yaml_loaded ensures base yaml doc exists")
        };

        if yaml_key != yaml_path {
            return Err(UnityAssetError::format(format!(
                "Hierarchy root YAML path mismatch after canonicalization: {} vs {}",
                yaml_path.display(),
                yaml_key.display()
            )));
        }

        let mut current_go = root_game_object.clone();
        for seg in segments {
            let go_obj = doc
                .entries()
                .iter()
                .find(|o| o.anchor == current_go.anchor && o.class_name == "GameObject")
                .ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "GameObject anchor not found: {} (file: {})",
                        current_go.anchor,
                        yaml_key.display()
                    ))
                })?;

            let component_ids = read_gameobject_component_file_ids(go_obj);
            let transform_anchor = ["RectTransform", "Transform"]
                .into_iter()
                .find_map(|want| {
                    component_ids.iter().find_map(|file_id| {
                        let anchor = file_id.to_string();
                        let component = doc.entries().iter().find(|o| o.anchor == anchor)?;
                        if component.class_name == want {
                            Some(component.anchor.clone())
                        } else {
                            None
                        }
                    })
                })
                .ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "Transform component not found on GameObject {} (file: {})",
                        current_go.anchor,
                        yaml_key.display()
                    ))
                })?;

            let transform = doc
                .entries()
                .iter()
                .find(|o| o.anchor == transform_anchor)
                .expect("transform_anchor comes from doc lookup");

            let mut matches: Vec<YamlObjectKey> = Vec::new();
            for child_tr_file_id in read_child_transform_file_ids(transform) {
                let child_tr_anchor = child_tr_file_id.to_string();
                let Some(child_tr) = doc.entries().iter().find(|o| o.anchor == child_tr_anchor)
                else {
                    continue;
                };
                let Some(child_go_file_id) = read_transform_gameobject_file_id(child_tr) else {
                    continue;
                };
                let child_go_anchor = child_go_file_id.to_string();
                let Some(child_go) = doc
                    .entries()
                    .iter()
                    .find(|o| o.anchor == child_go_anchor && o.class_name == "GameObject")
                else {
                    continue;
                };
                let Some(name) = child_go.get("m_Name").and_then(|v| v.as_str()) else {
                    continue;
                };
                if name == seg {
                    matches.push(YamlObjectKey {
                        path: yaml_key.clone(),
                        anchor: child_go_anchor,
                    });
                }
            }

            match matches.as_slice() {
                [only] => current_go = only.clone(),
                [] => {
                    return Err(UnityAssetError::format(format!(
                        "Child GameObject not found under {}: {} (file: {})",
                        current_go.anchor,
                        seg,
                        yaml_key.display()
                    )));
                }
                many => {
                    return Err(UnityAssetError::format(format!(
                        "Child GameObject name is not unique under {}: {} (matches={}, file: {})",
                        current_go.anchor,
                        seg,
                        many.len(),
                        yaml_key.display()
                    )));
                }
            }
        }

        Ok(current_go)
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

    pub fn find_yaml_monobehaviour_key_by_required_fields(
        &mut self,
        game_object: &YamlObjectKey,
        required_paths: &[&str],
    ) -> Result<YamlObjectKey> {
        if required_paths.is_empty() {
            return Err(UnityAssetError::format("No required fields provided"));
        }

        let (yaml_key, doc) = self.yaml_doc_for_read(&game_object.path)?;
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

        let mut matches: Vec<YamlObjectKey> = Vec::new();
        for file_id in read_gameobject_component_file_ids(go) {
            let anchor = file_id.to_string();
            let Some(component) = doc.entries().iter().find(|o| o.anchor == anchor) else {
                continue;
            };
            if component.class_name != "MonoBehaviour" {
                continue;
            }
            if required_paths.iter().all(|p| {
                component.get(*p).is_some()
                    || super::pptr_path::get_value_at_path(component, p).is_some()
            }) {
                matches.push(YamlObjectKey {
                    path: yaml_key.clone(),
                    anchor,
                });
            }
        }

        match matches.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(UnityAssetError::format(format!(
                "MonoBehaviour not found on GameObject {} with required fields {:?} (file: {})",
                game_object.anchor,
                required_paths,
                yaml_key.display()
            ))),
            many => Err(UnityAssetError::format(format!(
                "MonoBehaviour required-field match is not unique on GameObject {} (fields={:?}, matches={}, file: {})",
                game_object.anchor,
                required_paths,
                many.len(),
                yaml_key.display()
            ))),
        }
    }

    pub fn find_yaml_canvas_key(&mut self, game_object: &YamlObjectKey) -> Result<YamlObjectKey> {
        self.find_yaml_component_key_by_class_name(game_object, "Canvas")
    }

    pub fn yaml_ui_canvas_set_render_mode(
        &mut self,
        canvas: &YamlObjectKey,
        render_mode: i64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(canvas, "m_RenderMode", UnityValue::Integer(render_mode))
    }

    pub fn yaml_ui_canvas_set_pixel_perfect(
        &mut self,
        canvas: &YamlObjectKey,
        pixel_perfect: bool,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            canvas,
            "m_PixelPerfect",
            UnityValue::Integer(if pixel_perfect { 1 } else { 0 }),
        )
    }

    pub fn yaml_ui_canvas_set_override_sorting(
        &mut self,
        canvas: &YamlObjectKey,
        override_sorting: bool,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            canvas,
            "m_OverrideSorting",
            UnityValue::Integer(if override_sorting { 1 } else { 0 }),
        )
    }

    pub fn yaml_ui_canvas_set_sorting_order(
        &mut self,
        canvas: &YamlObjectKey,
        sorting_order: i64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            canvas,
            "m_SortingOrder",
            UnityValue::Integer(sorting_order),
        )
    }

    pub fn find_yaml_canvas_scaler_key(
        &mut self,
        game_object: &YamlObjectKey,
    ) -> Result<YamlObjectKey> {
        self.find_yaml_monobehaviour_key_by_required_fields(
            game_object,
            &[
                "m_UiScaleMode",
                "m_ReferenceResolution",
                "m_MatchWidthOrHeight",
            ],
        )
    }

    pub fn yaml_ui_canvas_scaler_set_ui_scale_mode(
        &mut self,
        canvas_scaler: &YamlObjectKey,
        ui_scale_mode: i64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            canvas_scaler,
            "m_UiScaleMode",
            UnityValue::Integer(ui_scale_mode),
        )
    }

    pub fn yaml_ui_canvas_scaler_set_reference_resolution(
        &mut self,
        canvas_scaler: &YamlObjectKey,
        x: f64,
        y: f64,
    ) -> Result<()> {
        self.set_yaml_vec2_at_key_path(canvas_scaler, "m_ReferenceResolution", x, y)
    }

    pub fn yaml_ui_canvas_scaler_set_screen_match_mode(
        &mut self,
        canvas_scaler: &YamlObjectKey,
        screen_match_mode: i64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            canvas_scaler,
            "m_ScreenMatchMode",
            UnityValue::Integer(screen_match_mode),
        )
    }

    pub fn yaml_ui_canvas_scaler_set_match_width_or_height(
        &mut self,
        canvas_scaler: &YamlObjectKey,
        match_width_or_height: f64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            canvas_scaler,
            "m_MatchWidthOrHeight",
            UnityValue::Float(match_width_or_height),
        )
    }

    pub fn yaml_ui_canvas_scaler_set_scale_factor(
        &mut self,
        canvas_scaler: &YamlObjectKey,
        scale_factor: f64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            canvas_scaler,
            "m_ScaleFactor",
            UnityValue::Float(scale_factor),
        )
    }

    pub fn yaml_ui_set_graphic_raycast_target(
        &mut self,
        component: &YamlObjectKey,
        enabled: bool,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path_first_match(
            component,
            &["m_RaycastTarget", "m_raycastTarget"],
            UnityValue::Integer(if enabled { 1 } else { 0 }),
        )
    }

    pub fn yaml_ui_set_image_sprite(
        &mut self,
        image_component: &YamlObjectKey,
        file_id: i64,
        guid_32_hex: Option<&str>,
        type_id: Option<i64>,
    ) -> Result<()> {
        self.set_yaml_pptr_at_key_path_first_match(
            image_component,
            &["m_Sprite"],
            file_id,
            guid_32_hex,
            type_id,
        )
    }

    pub fn yaml_ui_set_raw_image_texture(
        &mut self,
        raw_image_component: &YamlObjectKey,
        file_id: i64,
        guid_32_hex: Option<&str>,
        type_id: Option<i64>,
    ) -> Result<()> {
        self.set_yaml_pptr_at_key_path_first_match(
            raw_image_component,
            &["m_Texture"],
            file_id,
            guid_32_hex,
            type_id,
        )
    }

    pub fn yaml_ui_set_graphic_color_rgba(
        &mut self,
        component: &YamlObjectKey,
        r: f64,
        g: f64,
        b: f64,
        a: f64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path_first_match(
            component,
            &["m_Color", "m_fontColor"],
            color_rgba_value(r, g, b, a),
        )
    }

    pub fn yaml_ui_set_text_string(&mut self, component: &YamlObjectKey, text: &str) -> Result<()> {
        self.set_yaml_string_at_key_path_first_match(component, &["m_Text", "m_text"], text)
    }

    pub fn yaml_ui_set_text_font_size(
        &mut self,
        component: &YamlObjectKey,
        size: i64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path_first_match(
            component,
            &["m_FontData.m_FontSize", "m_fontSize"],
            UnityValue::Integer(size),
        )
    }

    pub fn find_yaml_button_key(&mut self, game_object: &YamlObjectKey) -> Result<YamlObjectKey> {
        self.find_yaml_monobehaviour_key_by_required_fields(
            game_object,
            &["m_OnClick", "m_Interactable"],
        )
    }

    pub fn yaml_ui_button_set_interactable(
        &mut self,
        button: &YamlObjectKey,
        interactable: bool,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path_first_match(
            button,
            &["m_Interactable"],
            UnityValue::Integer(if interactable { 1 } else { 0 }),
        )
    }

    pub fn yaml_ui_button_clear_on_click(&mut self, button: &YamlObjectKey) -> Result<()> {
        self.env_mut()
            .edit_yaml_object_anchor(&button.path, button.anchor.as_str(), |class| {
                let calls = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnClick.m_PersistentCalls.m_Calls",
                )?;
                *calls = UnityValue::Array(Vec::new());
                Ok(())
            })
    }

    pub fn yaml_ui_button_add_on_click_call(
        &mut self,
        button: &YamlObjectKey,
        target_file_id: i64,
        target_guid_32_hex: Option<&str>,
        target_type_id: Option<i64>,
        method_name: &str,
    ) -> Result<()> {
        let target_guid_32_hex = target_guid_32_hex.map(|s| s.trim().to_ascii_lowercase());
        let target = yaml_pptr_value(
            target_file_id,
            target_guid_32_hex.as_deref(),
            target_type_id,
        );

        self.env_mut()
            .edit_yaml_object_anchor(&button.path, button.anchor.as_str(), |class| {
                let calls_value = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnClick.m_PersistentCalls.m_Calls",
                )?;
                let calls = ensure_array_mut(calls_value);

                let args: UnityValue = UnityValue::Object(
                    [
                        (
                            "m_ObjectArgument".to_string(),
                            yaml_pptr_value(0, None, None),
                        ),
                        (
                            "m_ObjectArgumentAssemblyTypeName".to_string(),
                            UnityValue::String(String::new()),
                        ),
                        ("m_IntArgument".to_string(), UnityValue::Integer(0)),
                        ("m_FloatArgument".to_string(), UnityValue::Float(0.0)),
                        (
                            "m_StringArgument".to_string(),
                            UnityValue::String(String::new()),
                        ),
                        ("m_BoolArgument".to_string(), UnityValue::Integer(0)),
                    ]
                    .into_iter()
                    .collect(),
                );

                let call: UnityValue = UnityValue::Object(
                    [
                        ("m_Target".to_string(), target),
                        (
                            "m_MethodName".to_string(),
                            UnityValue::String(method_name.to_string()),
                        ),
                        // PersistentListenerMode.Void
                        ("m_Mode".to_string(), UnityValue::Integer(1)),
                        ("m_Arguments".to_string(), args),
                        // UnityEventCallState.RuntimeOnly
                        ("m_CallState".to_string(), UnityValue::Integer(2)),
                    ]
                    .into_iter()
                    .collect(),
                );

                calls.push(call);
                Ok(())
            })
    }

    pub fn yaml_ui_button_add_on_click_target_anchor(
        &mut self,
        button: &YamlObjectKey,
        target_anchor: &str,
        method_name: &str,
    ) -> Result<()> {
        let target_file_id = target_anchor.trim().parse::<i64>().map_err(|e| {
            UnityAssetError::format(format!(
                "Invalid YAML anchor fileID for onClick target: {:?} ({})",
                target_anchor, e
            ))
        })?;
        self.yaml_ui_button_add_on_click_call(button, target_file_id, None, None, method_name)
    }

    pub fn find_yaml_layout_group_key(
        &mut self,
        game_object: &YamlObjectKey,
    ) -> Result<YamlObjectKey> {
        self.find_yaml_monobehaviour_key_by_required_fields(
            game_object,
            &["m_Padding", "m_ChildAlignment"],
        )
    }

    pub fn yaml_ui_layout_group_set_padding(
        &mut self,
        layout_group: &YamlObjectKey,
        left: i64,
        right: i64,
        top: i64,
        bottom: i64,
    ) -> Result<()> {
        self.env_mut().edit_yaml_object_anchor(
            &layout_group.path,
            layout_group.anchor.as_str(),
            |class| {
                super::pptr_path::set_value_at_path(
                    class,
                    "m_Padding.m_Left",
                    UnityValue::Integer(left),
                )?;
                super::pptr_path::set_value_at_path(
                    class,
                    "m_Padding.m_Right",
                    UnityValue::Integer(right),
                )?;
                super::pptr_path::set_value_at_path(
                    class,
                    "m_Padding.m_Top",
                    UnityValue::Integer(top),
                )?;
                super::pptr_path::set_value_at_path(
                    class,
                    "m_Padding.m_Bottom",
                    UnityValue::Integer(bottom),
                )?;
                Ok(())
            },
        )
    }

    pub fn yaml_ui_layout_group_set_child_alignment(
        &mut self,
        layout_group: &YamlObjectKey,
        alignment: i64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            layout_group,
            "m_ChildAlignment",
            UnityValue::Integer(alignment),
        )
    }

    pub fn yaml_ui_layout_group_set_spacing(
        &mut self,
        layout_group: &YamlObjectKey,
        spacing: f64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(layout_group, "m_Spacing", UnityValue::Float(spacing))
    }

    pub fn yaml_ui_layout_group_set_child_control(
        &mut self,
        layout_group: &YamlObjectKey,
        width: bool,
        height: bool,
    ) -> Result<()> {
        self.env_mut().edit_yaml_object_anchor(
            &layout_group.path,
            layout_group.anchor.as_str(),
            |class| {
                super::pptr_path::set_value_at_path(
                    class,
                    "m_ChildControlWidth",
                    UnityValue::Integer(if width { 1 } else { 0 }),
                )?;
                super::pptr_path::set_value_at_path(
                    class,
                    "m_ChildControlHeight",
                    UnityValue::Integer(if height { 1 } else { 0 }),
                )?;
                Ok(())
            },
        )
    }

    pub fn yaml_ui_layout_group_set_child_force_expand(
        &mut self,
        layout_group: &YamlObjectKey,
        width: bool,
        height: bool,
    ) -> Result<()> {
        self.env_mut().edit_yaml_object_anchor(
            &layout_group.path,
            layout_group.anchor.as_str(),
            |class| {
                super::pptr_path::set_value_at_path(
                    class,
                    "m_ChildForceExpandWidth",
                    UnityValue::Integer(if width { 1 } else { 0 }),
                )?;
                super::pptr_path::set_value_at_path(
                    class,
                    "m_ChildForceExpandHeight",
                    UnityValue::Integer(if height { 1 } else { 0 }),
                )?;
                Ok(())
            },
        )
    }

    pub fn find_yaml_toggle_key(&mut self, game_object: &YamlObjectKey) -> Result<YamlObjectKey> {
        self.find_yaml_monobehaviour_key_by_required_fields(
            game_object,
            &["m_IsOn", "m_Interactable", "m_OnValueChanged"],
        )
    }

    pub fn yaml_ui_toggle_set_is_on(&mut self, toggle: &YamlObjectKey, is_on: bool) -> Result<()> {
        self.set_yaml_value_at_key_path(
            toggle,
            "m_IsOn",
            UnityValue::Integer(if is_on { 1 } else { 0 }),
        )
    }

    pub fn yaml_ui_toggle_set_interactable(
        &mut self,
        toggle: &YamlObjectKey,
        interactable: bool,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            toggle,
            "m_Interactable",
            UnityValue::Integer(if interactable { 1 } else { 0 }),
        )
    }

    pub fn yaml_ui_toggle_clear_on_value_changed(&mut self, toggle: &YamlObjectKey) -> Result<()> {
        self.env_mut()
            .edit_yaml_object_anchor(&toggle.path, toggle.anchor.as_str(), |class| {
                let calls = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnValueChanged.m_PersistentCalls.m_Calls",
                )?;
                *calls = UnityValue::Array(Vec::new());
                Ok(())
            })
    }

    pub fn yaml_ui_toggle_add_on_value_changed_call(
        &mut self,
        toggle: &YamlObjectKey,
        target_file_id: i64,
        target_guid_32_hex: Option<&str>,
        target_type_id: Option<i64>,
        method_name: &str,
    ) -> Result<()> {
        let target_guid_32_hex = target_guid_32_hex.map(|s| s.trim().to_ascii_lowercase());
        let target = yaml_pptr_value(
            target_file_id,
            target_guid_32_hex.as_deref(),
            target_type_id,
        );

        self.env_mut()
            .edit_yaml_object_anchor(&toggle.path, toggle.anchor.as_str(), |class| {
                let calls_value = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnValueChanged.m_PersistentCalls.m_Calls",
                )?;
                let calls = ensure_array_mut(calls_value);

                let args: UnityValue = UnityValue::Object(
                    [
                        (
                            "m_ObjectArgument".to_string(),
                            yaml_pptr_value(0, None, None),
                        ),
                        (
                            "m_ObjectArgumentAssemblyTypeName".to_string(),
                            UnityValue::String(String::new()),
                        ),
                        ("m_IntArgument".to_string(), UnityValue::Integer(0)),
                        ("m_FloatArgument".to_string(), UnityValue::Float(0.0)),
                        (
                            "m_StringArgument".to_string(),
                            UnityValue::String(String::new()),
                        ),
                        ("m_BoolArgument".to_string(), UnityValue::Integer(0)),
                    ]
                    .into_iter()
                    .collect(),
                );

                let call: UnityValue = UnityValue::Object(
                    [
                        ("m_Target".to_string(), target),
                        (
                            "m_MethodName".to_string(),
                            UnityValue::String(method_name.to_string()),
                        ),
                        // Best-effort: treat as event-defined so Unity can bind "dynamic" listeners.
                        ("m_Mode".to_string(), UnityValue::Integer(0)),
                        ("m_Arguments".to_string(), args),
                        // UnityEventCallState.RuntimeOnly
                        ("m_CallState".to_string(), UnityValue::Integer(2)),
                    ]
                    .into_iter()
                    .collect(),
                );

                calls.push(call);
                Ok(())
            })
    }

    pub fn yaml_ui_toggle_add_on_value_changed_target_anchor(
        &mut self,
        toggle: &YamlObjectKey,
        target_anchor: &str,
        method_name: &str,
    ) -> Result<()> {
        let target_file_id = target_anchor.trim().parse::<i64>().map_err(|e| {
            UnityAssetError::format(format!(
                "Invalid YAML anchor fileID for onValueChanged target: {:?} ({})",
                target_anchor, e
            ))
        })?;
        self.yaml_ui_toggle_add_on_value_changed_call(
            toggle,
            target_file_id,
            None,
            None,
            method_name,
        )
    }

    pub fn find_yaml_slider_key(&mut self, game_object: &YamlObjectKey) -> Result<YamlObjectKey> {
        self.find_yaml_monobehaviour_key_by_required_fields(
            game_object,
            &["m_Value", "m_Interactable", "m_OnValueChanged"],
        )
    }

    pub fn yaml_ui_slider_set_value(&mut self, slider: &YamlObjectKey, value: f64) -> Result<()> {
        self.set_yaml_value_at_key_path(slider, "m_Value", UnityValue::Float(value))
    }

    pub fn yaml_ui_slider_set_min_max(
        &mut self,
        slider: &YamlObjectKey,
        min: f64,
        max: f64,
    ) -> Result<()> {
        self.env_mut()
            .edit_yaml_object_anchor(&slider.path, slider.anchor.as_str(), |class| {
                super::pptr_path::set_value_at_path(class, "m_MinValue", UnityValue::Float(min))?;
                super::pptr_path::set_value_at_path(class, "m_MaxValue", UnityValue::Float(max))?;
                Ok(())
            })
    }

    pub fn yaml_ui_slider_set_whole_numbers(
        &mut self,
        slider: &YamlObjectKey,
        whole_numbers: bool,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            slider,
            "m_WholeNumbers",
            UnityValue::Integer(if whole_numbers { 1 } else { 0 }),
        )
    }

    pub fn yaml_ui_slider_set_interactable(
        &mut self,
        slider: &YamlObjectKey,
        interactable: bool,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            slider,
            "m_Interactable",
            UnityValue::Integer(if interactable { 1 } else { 0 }),
        )
    }

    pub fn yaml_ui_slider_clear_on_value_changed(&mut self, slider: &YamlObjectKey) -> Result<()> {
        self.env_mut()
            .edit_yaml_object_anchor(&slider.path, slider.anchor.as_str(), |class| {
                let calls = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnValueChanged.m_PersistentCalls.m_Calls",
                )?;
                *calls = UnityValue::Array(Vec::new());
                Ok(())
            })
    }

    pub fn yaml_ui_slider_add_on_value_changed_call(
        &mut self,
        slider: &YamlObjectKey,
        target_file_id: i64,
        target_guid_32_hex: Option<&str>,
        target_type_id: Option<i64>,
        method_name: &str,
    ) -> Result<()> {
        let target_guid_32_hex = target_guid_32_hex.map(|s| s.trim().to_ascii_lowercase());
        let target = yaml_pptr_value(
            target_file_id,
            target_guid_32_hex.as_deref(),
            target_type_id,
        );

        self.env_mut()
            .edit_yaml_object_anchor(&slider.path, slider.anchor.as_str(), |class| {
                let calls_value = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnValueChanged.m_PersistentCalls.m_Calls",
                )?;
                let calls = ensure_array_mut(calls_value);

                let args: UnityValue = UnityValue::Object(
                    [
                        (
                            "m_ObjectArgument".to_string(),
                            yaml_pptr_value(0, None, None),
                        ),
                        (
                            "m_ObjectArgumentAssemblyTypeName".to_string(),
                            UnityValue::String(String::new()),
                        ),
                        ("m_IntArgument".to_string(), UnityValue::Integer(0)),
                        ("m_FloatArgument".to_string(), UnityValue::Float(0.0)),
                        (
                            "m_StringArgument".to_string(),
                            UnityValue::String(String::new()),
                        ),
                        ("m_BoolArgument".to_string(), UnityValue::Integer(0)),
                    ]
                    .into_iter()
                    .collect(),
                );

                let call: UnityValue = UnityValue::Object(
                    [
                        ("m_Target".to_string(), target),
                        (
                            "m_MethodName".to_string(),
                            UnityValue::String(method_name.to_string()),
                        ),
                        // Best-effort: treat as event-defined so Unity can bind "dynamic" listeners.
                        ("m_Mode".to_string(), UnityValue::Integer(0)),
                        ("m_Arguments".to_string(), args),
                        // UnityEventCallState.RuntimeOnly
                        ("m_CallState".to_string(), UnityValue::Integer(2)),
                    ]
                    .into_iter()
                    .collect(),
                );

                calls.push(call);
                Ok(())
            })
    }

    pub fn yaml_ui_slider_add_on_value_changed_target_anchor(
        &mut self,
        slider: &YamlObjectKey,
        target_anchor: &str,
        method_name: &str,
    ) -> Result<()> {
        let target_file_id = target_anchor.trim().parse::<i64>().map_err(|e| {
            UnityAssetError::format(format!(
                "Invalid YAML anchor fileID for onValueChanged target: {:?} ({})",
                target_anchor, e
            ))
        })?;
        self.yaml_ui_slider_add_on_value_changed_call(
            slider,
            target_file_id,
            None,
            None,
            method_name,
        )
    }

    pub fn find_yaml_dropdown_key(&mut self, game_object: &YamlObjectKey) -> Result<YamlObjectKey> {
        self.find_yaml_monobehaviour_key_by_required_fields(
            game_object,
            &["m_Value", "m_Interactable", "m_OnValueChanged"],
        )
    }

    pub fn yaml_ui_dropdown_set_value(
        &mut self,
        dropdown: &YamlObjectKey,
        value: i64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(dropdown, "m_Value", UnityValue::Integer(value))
    }

    pub fn yaml_ui_dropdown_set_interactable(
        &mut self,
        dropdown: &YamlObjectKey,
        interactable: bool,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            dropdown,
            "m_Interactable",
            UnityValue::Integer(if interactable { 1 } else { 0 }),
        )
    }

    pub fn yaml_ui_dropdown_clear_on_value_changed(
        &mut self,
        dropdown: &YamlObjectKey,
    ) -> Result<()> {
        self.env_mut()
            .edit_yaml_object_anchor(&dropdown.path, dropdown.anchor.as_str(), |class| {
                let calls = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnValueChanged.m_PersistentCalls.m_Calls",
                )?;
                *calls = UnityValue::Array(Vec::new());
                Ok(())
            })
    }

    pub fn yaml_ui_dropdown_add_on_value_changed_call(
        &mut self,
        dropdown: &YamlObjectKey,
        target_file_id: i64,
        target_guid_32_hex: Option<&str>,
        target_type_id: Option<i64>,
        method_name: &str,
    ) -> Result<()> {
        let target_guid_32_hex = target_guid_32_hex.map(|s| s.trim().to_ascii_lowercase());
        let target = yaml_pptr_value(
            target_file_id,
            target_guid_32_hex.as_deref(),
            target_type_id,
        );

        self.env_mut()
            .edit_yaml_object_anchor(&dropdown.path, dropdown.anchor.as_str(), |class| {
                let calls_value = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnValueChanged.m_PersistentCalls.m_Calls",
                )?;
                let calls = ensure_array_mut(calls_value);
                calls.push(unity_event_persistent_call(target, method_name, 0));
                Ok(())
            })
    }

    pub fn yaml_ui_dropdown_add_on_value_changed_target_anchor(
        &mut self,
        dropdown: &YamlObjectKey,
        target_anchor: &str,
        method_name: &str,
    ) -> Result<()> {
        let target_file_id = target_anchor.trim().parse::<i64>().map_err(|e| {
            UnityAssetError::format(format!(
                "Invalid YAML anchor fileID for dropdown onValueChanged target: {:?} ({})",
                target_anchor, e
            ))
        })?;
        self.yaml_ui_dropdown_add_on_value_changed_call(
            dropdown,
            target_file_id,
            None,
            None,
            method_name,
        )
    }

    pub fn find_yaml_input_field_key(
        &mut self,
        game_object: &YamlObjectKey,
    ) -> Result<YamlObjectKey> {
        self.find_yaml_monobehaviour_key_by_required_fields(
            game_object,
            &["m_Text", "m_OnValueChanged", "m_OnEndEdit"],
        )
    }

    pub fn yaml_ui_input_field_set_text(
        &mut self,
        input: &YamlObjectKey,
        text: &str,
    ) -> Result<()> {
        self.set_yaml_string_at_key_path_first_match(input, &["m_Text", "m_text"], text)
    }

    pub fn yaml_ui_input_field_set_interactable(
        &mut self,
        input: &YamlObjectKey,
        interactable: bool,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            input,
            "m_Interactable",
            UnityValue::Integer(if interactable { 1 } else { 0 }),
        )
    }

    pub fn yaml_ui_input_field_clear_on_value_changed(
        &mut self,
        input: &YamlObjectKey,
    ) -> Result<()> {
        self.env_mut()
            .edit_yaml_object_anchor(&input.path, input.anchor.as_str(), |class| {
                let calls = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnValueChanged.m_PersistentCalls.m_Calls",
                )?;
                *calls = UnityValue::Array(Vec::new());
                Ok(())
            })
    }

    pub fn yaml_ui_input_field_add_on_value_changed_call(
        &mut self,
        input: &YamlObjectKey,
        target_file_id: i64,
        target_guid_32_hex: Option<&str>,
        target_type_id: Option<i64>,
        method_name: &str,
    ) -> Result<()> {
        let target_guid_32_hex = target_guid_32_hex.map(|s| s.trim().to_ascii_lowercase());
        let target = yaml_pptr_value(
            target_file_id,
            target_guid_32_hex.as_deref(),
            target_type_id,
        );

        self.env_mut()
            .edit_yaml_object_anchor(&input.path, input.anchor.as_str(), |class| {
                let calls_value = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnValueChanged.m_PersistentCalls.m_Calls",
                )?;
                let calls = ensure_array_mut(calls_value);
                calls.push(unity_event_persistent_call(target, method_name, 0));
                Ok(())
            })
    }

    pub fn yaml_ui_input_field_add_on_value_changed_target_anchor(
        &mut self,
        input: &YamlObjectKey,
        target_anchor: &str,
        method_name: &str,
    ) -> Result<()> {
        let target_file_id = target_anchor.trim().parse::<i64>().map_err(|e| {
            UnityAssetError::format(format!(
                "Invalid YAML anchor fileID for input onValueChanged target: {:?} ({})",
                target_anchor, e
            ))
        })?;
        self.yaml_ui_input_field_add_on_value_changed_call(
            input,
            target_file_id,
            None,
            None,
            method_name,
        )
    }

    pub fn yaml_ui_input_field_clear_on_end_edit(&mut self, input: &YamlObjectKey) -> Result<()> {
        self.env_mut()
            .edit_yaml_object_anchor(&input.path, input.anchor.as_str(), |class| {
                let calls = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnEndEdit.m_PersistentCalls.m_Calls",
                )?;
                *calls = UnityValue::Array(Vec::new());
                Ok(())
            })
    }

    pub fn yaml_ui_input_field_add_on_end_edit_call(
        &mut self,
        input: &YamlObjectKey,
        target_file_id: i64,
        target_guid_32_hex: Option<&str>,
        target_type_id: Option<i64>,
        method_name: &str,
    ) -> Result<()> {
        let target_guid_32_hex = target_guid_32_hex.map(|s| s.trim().to_ascii_lowercase());
        let target = yaml_pptr_value(
            target_file_id,
            target_guid_32_hex.as_deref(),
            target_type_id,
        );

        self.env_mut()
            .edit_yaml_object_anchor(&input.path, input.anchor.as_str(), |class| {
                let calls_value = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnEndEdit.m_PersistentCalls.m_Calls",
                )?;
                let calls = ensure_array_mut(calls_value);
                calls.push(unity_event_persistent_call(target, method_name, 0));
                Ok(())
            })
    }

    pub fn yaml_ui_input_field_add_on_end_edit_target_anchor(
        &mut self,
        input: &YamlObjectKey,
        target_anchor: &str,
        method_name: &str,
    ) -> Result<()> {
        let target_file_id = target_anchor.trim().parse::<i64>().map_err(|e| {
            UnityAssetError::format(format!(
                "Invalid YAML anchor fileID for input onEndEdit target: {:?} ({})",
                target_anchor, e
            ))
        })?;
        self.yaml_ui_input_field_add_on_end_edit_call(
            input,
            target_file_id,
            None,
            None,
            method_name,
        )
    }

    pub fn find_yaml_tmp_input_field_key(
        &mut self,
        game_object: &YamlObjectKey,
    ) -> Result<YamlObjectKey> {
        self.find_yaml_monobehaviour_key_by_required_fields(
            game_object,
            &[
                "m_Text",
                "m_TextComponent",
                "m_OnValueChanged",
                "m_OnEndEdit",
            ],
        )
    }

    pub fn yaml_ui_tmp_input_field_set_text(
        &mut self,
        tmp_input: &YamlObjectKey,
        text: &str,
    ) -> Result<()> {
        self.set_yaml_string_at_key_path_first_match(tmp_input, &["m_Text", "m_text"], text)
    }

    pub fn yaml_ui_tmp_input_field_set_interactable(
        &mut self,
        tmp_input: &YamlObjectKey,
        interactable: bool,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            tmp_input,
            "m_Interactable",
            UnityValue::Integer(if interactable { 1 } else { 0 }),
        )
    }

    pub fn yaml_ui_tmp_input_field_clear_on_value_changed(
        &mut self,
        tmp_input: &YamlObjectKey,
    ) -> Result<()> {
        self.env_mut().edit_yaml_object_anchor(
            &tmp_input.path,
            tmp_input.anchor.as_str(),
            |class| {
                let calls = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnValueChanged.m_PersistentCalls.m_Calls",
                )?;
                *calls = UnityValue::Array(Vec::new());
                Ok(())
            },
        )
    }

    pub fn yaml_ui_tmp_input_field_add_on_value_changed_call(
        &mut self,
        tmp_input: &YamlObjectKey,
        target_file_id: i64,
        target_guid_32_hex: Option<&str>,
        target_type_id: Option<i64>,
        method_name: &str,
    ) -> Result<()> {
        let target_guid_32_hex = target_guid_32_hex.map(|s| s.trim().to_ascii_lowercase());
        let target = yaml_pptr_value(
            target_file_id,
            target_guid_32_hex.as_deref(),
            target_type_id,
        );

        self.env_mut().edit_yaml_object_anchor(
            &tmp_input.path,
            tmp_input.anchor.as_str(),
            |class| {
                let calls_value = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnValueChanged.m_PersistentCalls.m_Calls",
                )?;
                let calls = ensure_array_mut(calls_value);
                calls.push(unity_event_persistent_call(target, method_name, 0));
                Ok(())
            },
        )
    }

    pub fn yaml_ui_tmp_input_field_add_on_value_changed_target_anchor(
        &mut self,
        tmp_input: &YamlObjectKey,
        target_anchor: &str,
        method_name: &str,
    ) -> Result<()> {
        let target_file_id = target_anchor.trim().parse::<i64>().map_err(|e| {
            UnityAssetError::format(format!(
                "Invalid YAML anchor fileID for TMP_InputField onValueChanged target: {:?} ({})",
                target_anchor, e
            ))
        })?;
        self.yaml_ui_tmp_input_field_add_on_value_changed_call(
            tmp_input,
            target_file_id,
            None,
            None,
            method_name,
        )
    }

    pub fn yaml_ui_tmp_input_field_clear_on_end_edit(
        &mut self,
        tmp_input: &YamlObjectKey,
    ) -> Result<()> {
        self.env_mut().edit_yaml_object_anchor(
            &tmp_input.path,
            tmp_input.anchor.as_str(),
            |class| {
                let calls = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnEndEdit.m_PersistentCalls.m_Calls",
                )?;
                *calls = UnityValue::Array(Vec::new());
                Ok(())
            },
        )
    }

    pub fn yaml_ui_tmp_input_field_add_on_end_edit_call(
        &mut self,
        tmp_input: &YamlObjectKey,
        target_file_id: i64,
        target_guid_32_hex: Option<&str>,
        target_type_id: Option<i64>,
        method_name: &str,
    ) -> Result<()> {
        let target_guid_32_hex = target_guid_32_hex.map(|s| s.trim().to_ascii_lowercase());
        let target = yaml_pptr_value(
            target_file_id,
            target_guid_32_hex.as_deref(),
            target_type_id,
        );

        self.env_mut().edit_yaml_object_anchor(
            &tmp_input.path,
            tmp_input.anchor.as_str(),
            |class| {
                let calls_value = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnEndEdit.m_PersistentCalls.m_Calls",
                )?;
                let calls = ensure_array_mut(calls_value);
                calls.push(unity_event_persistent_call(target, method_name, 0));
                Ok(())
            },
        )
    }

    pub fn yaml_ui_tmp_input_field_add_on_end_edit_target_anchor(
        &mut self,
        tmp_input: &YamlObjectKey,
        target_anchor: &str,
        method_name: &str,
    ) -> Result<()> {
        let target_file_id = target_anchor.trim().parse::<i64>().map_err(|e| {
            UnityAssetError::format(format!(
                "Invalid YAML anchor fileID for TMP_InputField onEndEdit target: {:?} ({})",
                target_anchor, e
            ))
        })?;
        self.yaml_ui_tmp_input_field_add_on_end_edit_call(
            tmp_input,
            target_file_id,
            None,
            None,
            method_name,
        )
    }

    pub fn find_yaml_scroll_rect_key(
        &mut self,
        game_object: &YamlObjectKey,
    ) -> Result<YamlObjectKey> {
        self.find_yaml_monobehaviour_key_by_required_fields(
            game_object,
            &[
                "m_Content",
                "m_Viewport",
                "m_OnValueChanged",
                "m_Horizontal",
                "m_Vertical",
            ],
        )
    }

    pub fn yaml_ui_scroll_rect_set_content_target_anchor(
        &mut self,
        scroll_rect: &YamlObjectKey,
        content_anchor: &str,
    ) -> Result<()> {
        self.set_yaml_pptr_to_yaml_anchor_at_key_path(scroll_rect, "m_Content", content_anchor)
    }

    pub fn yaml_ui_scroll_rect_set_viewport_target_anchor(
        &mut self,
        scroll_rect: &YamlObjectKey,
        viewport_anchor: &str,
    ) -> Result<()> {
        self.set_yaml_pptr_to_yaml_anchor_at_key_path(scroll_rect, "m_Viewport", viewport_anchor)
    }

    pub fn yaml_ui_scroll_rect_set_horizontal(
        &mut self,
        scroll_rect: &YamlObjectKey,
        enabled: bool,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            scroll_rect,
            "m_Horizontal",
            UnityValue::Integer(if enabled { 1 } else { 0 }),
        )
    }

    pub fn yaml_ui_scroll_rect_set_vertical(
        &mut self,
        scroll_rect: &YamlObjectKey,
        enabled: bool,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            scroll_rect,
            "m_Vertical",
            UnityValue::Integer(if enabled { 1 } else { 0 }),
        )
    }

    pub fn yaml_ui_scroll_rect_set_normalized_position(
        &mut self,
        scroll_rect: &YamlObjectKey,
        x: f64,
        y: f64,
    ) -> Result<()> {
        self.set_yaml_vec2_at_key_path(scroll_rect, "m_NormalizedPosition", x, y)
    }

    pub fn yaml_ui_scroll_rect_set_velocity(
        &mut self,
        scroll_rect: &YamlObjectKey,
        x: f64,
        y: f64,
    ) -> Result<()> {
        self.set_yaml_vec2_at_key_path(scroll_rect, "m_Velocity", x, y)
    }

    pub fn yaml_ui_scroll_rect_set_scroll_sensitivity(
        &mut self,
        scroll_rect: &YamlObjectKey,
        sensitivity: f64,
    ) -> Result<()> {
        self.set_yaml_value_at_key_path(
            scroll_rect,
            "m_ScrollSensitivity",
            UnityValue::Float(sensitivity),
        )
    }

    pub fn yaml_ui_scroll_rect_clear_on_value_changed(
        &mut self,
        scroll_rect: &YamlObjectKey,
    ) -> Result<()> {
        self.env_mut().edit_yaml_object_anchor(
            &scroll_rect.path,
            scroll_rect.anchor.as_str(),
            |class| {
                let calls = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnValueChanged.m_PersistentCalls.m_Calls",
                )?;
                *calls = UnityValue::Array(Vec::new());
                Ok(())
            },
        )
    }

    pub fn yaml_ui_scroll_rect_add_on_value_changed_call(
        &mut self,
        scroll_rect: &YamlObjectKey,
        target_file_id: i64,
        target_guid_32_hex: Option<&str>,
        target_type_id: Option<i64>,
        method_name: &str,
    ) -> Result<()> {
        let target_guid_32_hex = target_guid_32_hex.map(|s| s.trim().to_ascii_lowercase());
        let target = yaml_pptr_value(
            target_file_id,
            target_guid_32_hex.as_deref(),
            target_type_id,
        );

        self.env_mut().edit_yaml_object_anchor(
            &scroll_rect.path,
            scroll_rect.anchor.as_str(),
            |class| {
                let calls_value = super::pptr_path::get_value_at_path_mut(
                    class,
                    "m_OnValueChanged.m_PersistentCalls.m_Calls",
                )?;
                let calls = ensure_array_mut(calls_value);
                calls.push(unity_event_persistent_call(target, method_name, 0));
                Ok(())
            },
        )
    }

    pub fn yaml_ui_scroll_rect_add_on_value_changed_target_anchor(
        &mut self,
        scroll_rect: &YamlObjectKey,
        target_anchor: &str,
        method_name: &str,
    ) -> Result<()> {
        let target_file_id = target_anchor.trim().parse::<i64>().map_err(|e| {
            UnityAssetError::format(format!(
                "Invalid YAML anchor fileID for ScrollRect onValueChanged target: {:?} ({})",
                target_anchor, e
            ))
        })?;
        self.yaml_ui_scroll_rect_add_on_value_changed_call(
            scroll_rect,
            target_file_id,
            None,
            None,
            method_name,
        )
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

    pub fn yaml_rect_transform_set_anchor_min(
        &mut self,
        rect_transform: &YamlObjectKey,
        x: f64,
        y: f64,
    ) -> Result<()> {
        self.set_yaml_vec2_at_key_path(rect_transform, "m_AnchorMin", x, y)
    }

    pub fn yaml_rect_transform_set_anchor_max(
        &mut self,
        rect_transform: &YamlObjectKey,
        x: f64,
        y: f64,
    ) -> Result<()> {
        self.set_yaml_vec2_at_key_path(rect_transform, "m_AnchorMax", x, y)
    }

    pub fn yaml_rect_transform_set_pivot(
        &mut self,
        rect_transform: &YamlObjectKey,
        x: f64,
        y: f64,
    ) -> Result<()> {
        self.set_yaml_vec2_at_key_path(rect_transform, "m_Pivot", x, y)
    }

    pub fn yaml_rect_transform_set_offset_min(
        &mut self,
        rect_transform: &YamlObjectKey,
        x: f64,
        y: f64,
    ) -> Result<()> {
        self.set_yaml_vec2_at_key_path(rect_transform, "m_OffsetMin", x, y)
    }

    pub fn yaml_rect_transform_set_offset_max(
        &mut self,
        rect_transform: &YamlObjectKey,
        x: f64,
        y: f64,
    ) -> Result<()> {
        self.set_yaml_vec2_at_key_path(rect_transform, "m_OffsetMax", x, y)
    }

    pub fn yaml_transform_set_local_position(
        &mut self,
        transform: &YamlObjectKey,
        x: f64,
        y: f64,
        z: f64,
    ) -> Result<()> {
        self.set_yaml_vec3_at_key_path(transform, "m_LocalPosition", x, y, z)
    }

    pub fn yaml_transform_set_local_scale(
        &mut self,
        transform: &YamlObjectKey,
        x: f64,
        y: f64,
        z: f64,
    ) -> Result<()> {
        self.set_yaml_vec3_at_key_path(transform, "m_LocalScale", x, y, z)
    }

    pub fn yaml_transform_set_local_rotation_quat(
        &mut self,
        transform: &YamlObjectKey,
        x: f64,
        y: f64,
        z: f64,
        w: f64,
    ) -> Result<()> {
        self.set_yaml_quat_at_key_path(transform, "m_LocalRotation", x, y, z, w)
    }

    pub fn yaml_reparent_gameobject(
        &mut self,
        child_game_object: &YamlObjectKey,
        new_parent_game_object: &YamlObjectKey,
    ) -> Result<()> {
        let child_yaml = canonicalize_if_exists(&child_game_object.path);
        let parent_yaml = canonicalize_if_exists(&new_parent_game_object.path);
        if child_yaml != parent_yaml {
            return Err(UnityAssetError::format(format!(
                "Reparent requires both objects to be in the same YAML file: child={} parent={}",
                child_yaml.display(),
                parent_yaml.display()
            )));
        }

        let child_go = YamlObjectKey {
            path: child_yaml.clone(),
            anchor: child_game_object.anchor.clone(),
        };
        let new_parent_go = YamlObjectKey {
            path: parent_yaml.clone(),
            anchor: new_parent_game_object.anchor.clone(),
        };

        let child_tr = self.find_yaml_transform_key_for_gameobject(&child_go)?;
        let new_parent_tr = self.find_yaml_transform_key_for_gameobject(&new_parent_go)?;
        let child_tr_id = child_tr.anchor.parse::<i64>().map_err(|e| {
            UnityAssetError::format(format!(
                "Invalid child Transform anchor fileID: {} ({})",
                child_tr.anchor, e
            ))
        })?;

        let (yaml_key, old_parent_tr_file_id) = {
            let (yaml_key, doc) = self.yaml_doc_for_read(&child_yaml)?;
            let child_tr_obj = doc
                .entries()
                .iter()
                .find(|o| o.anchor == child_tr.anchor)
                .ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "Child Transform anchor not found: {} (file: {})",
                        child_tr.anchor,
                        yaml_key.display()
                    ))
                })?;
            (
                yaml_key,
                read_transform_parent_file_id(child_tr_obj).filter(|v| *v != 0),
            )
        };

        // 1) Update child's parent pointer.
        self.set_yaml_pptr_to_yaml_anchor_at_key_path(
            &child_tr,
            "m_Father",
            new_parent_tr.anchor.as_str(),
        )?;

        // 2) Remove child from old parent's m_Children.
        if let Some(old_parent_tr_file_id) = old_parent_tr_file_id {
            let old_parent_tr_key = YamlObjectKey {
                path: yaml_key.clone(),
                anchor: old_parent_tr_file_id.to_string(),
            };

            self.env_mut().edit_yaml_object_anchor(
                &yaml_key,
                old_parent_tr_key.anchor.as_str(),
                |class| {
                    let mut children: Vec<i64> = read_child_transform_file_ids(class);
                    children.retain(|v| *v != child_tr_id);
                    let value = UnityValue::Array(
                        children
                            .into_iter()
                            .map(|id| yaml_pptr_value(id, None, None))
                            .collect(),
                    );
                    super::pptr_path::set_value_at_path(class, "m_Children", value)
                },
            )?;
        }

        // 3) Add child to new parent's m_Children (append).
        self.env_mut().edit_yaml_object_anchor(
            &yaml_key,
            new_parent_tr.anchor.as_str(),
            |class| {
                let mut children: Vec<i64> = read_child_transform_file_ids(class);
                if !children.contains(&child_tr_id) {
                    children.push(child_tr_id);
                }

                let value = UnityValue::Array(
                    children
                        .into_iter()
                        .map(|id| yaml_pptr_value(id, None, None))
                        .collect(),
                );
                super::pptr_path::set_value_at_path(class, "m_Children", value)
            },
        )?;

        Ok(())
    }
}
