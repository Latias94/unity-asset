use super::edit::{EnvironmentEditSession, StreamedResourceWrite};
use super::{BinaryObjectKey, Result};
use unity_asset_core::{UnityAssetError, UnityClass, UnityValue};

fn ensure_object_field(class: &mut UnityClass, field_name: &str) {
    if class.get(field_name).is_some() {
        return;
    }
    class.set(
        field_name.to_string(),
        UnityValue::Object(Default::default()),
    );
}

fn clear_bytes_field(class: &mut UnityClass, field_name: &str) {
    if let Some(v) = class.get_mut(field_name) {
        *v = UnityValue::Bytes(Vec::new());
    }
}

fn clear_mesh_vertex_data_fields(vertex_data: &mut UnityValue) {
    let UnityValue::Object(map) = vertex_data else {
        return;
    };

    if let Some(v) = map.get_mut("m_DataSize") {
        *v = UnityValue::Integer(0);
    }
    if let Some(v) = map.get_mut("m_Data") {
        *v = UnityValue::Bytes(Vec::new());
    }
}

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

fn ensure_pptr_field(class: &mut UnityClass, field_name: &str) {
    ensure_object_field(class, field_name);
    match class.get_mut(field_name) {
        Some(UnityValue::Object(_)) => {}
        Some(other) => *other = UnityValue::Object(Default::default()),
        None => {}
    }
}

fn apply_pptr_field(class: &mut UnityClass, field_name: &str, file_id: i32, path_id: i64) {
    ensure_pptr_field(class, field_name);
    if let Some(v) = class.get_mut(field_name) {
        super::pptr_path::write_pptr(v, file_id, path_id);
    }
}

fn pptr_value(file_id: i32, path_id: i64) -> UnityValue {
    let mut v = UnityValue::Object(Default::default());
    super::pptr_path::write_pptr(&mut v, file_id, path_id);
    v
}

fn ensure_object_child<'a>(value: &'a mut UnityValue, key: &str) -> &'a mut UnityValue {
    if !matches!(value, UnityValue::Object(_)) {
        *value = UnityValue::Object(Default::default());
    }
    let UnityValue::Object(map) = value else {
        unreachable!();
    };
    map.entry(key.to_string())
        .or_insert_with(|| UnityValue::Object(Default::default()))
}

fn ensure_array_child<'a>(value: &'a mut UnityValue, key: &str) -> &'a mut Vec<UnityValue> {
    let child = ensure_object_child(value, key);
    if !matches!(child, UnityValue::Array(_)) {
        *child = UnityValue::Array(Vec::new());
    }
    match child {
        UnityValue::Array(v) => v,
        _ => unreachable!(),
    }
}

fn property_key_name(v: &UnityValue) -> Option<&str> {
    match v {
        UnityValue::String(s) => Some(s.as_str()),
        UnityValue::Object(map) => map.get("name").and_then(|v| v.as_str()),
        _ => None,
    }
}

fn pair_first_second_mut<'a>(
    v: &'a mut UnityValue,
) -> Option<(&'a mut UnityValue, &'a mut UnityValue)> {
    match v {
        UnityValue::Array(arr) if arr.len() == 2 => {
            // SAFETY: split borrow
            let (a, b) = arr.split_at_mut(1);
            Some((&mut a[0], &mut b[0]))
        }
        UnityValue::Object(map) => {
            // Unity TypeTree pair children are typically named `first`/`second`.
            let first = map.get_mut("first")? as *mut UnityValue;
            let second = map.get_mut("second")? as *mut UnityValue;
            // SAFETY: different keys, so distinct entries.
            unsafe { Some((&mut *first, &mut *second)) }
        }
        _ => None,
    }
}

pub(crate) fn apply_material_set_texenv_texture_pptr(
    class: &mut UnityClass,
    property_name: &str,
    file_id: i32,
    path_id: i64,
) -> Result<()> {
    ensure_object_field(class, "m_SavedProperties");
    let Some(saved) = class.get_mut("m_SavedProperties") else {
        unreachable!();
    };

    let tex_envs = ensure_array_child(saved, "m_TexEnvs");

    for entry in tex_envs.iter_mut() {
        let Some((first, second)) = pair_first_second_mut(entry) else {
            continue;
        };
        let Some(name) = property_key_name(first) else {
            continue;
        };
        if name != property_name {
            continue;
        }

        let tex_env = ensure_object_child(second, "m_Texture");
        super::pptr_path::write_pptr(tex_env, file_id, path_id);
        return Ok(());
    }

    // Not found: append a new entry (string key variant).
    let mut tex_env = UnityValue::Object(Default::default());
    tex_env
        .as_object_mut()
        .unwrap()
        .insert("m_Texture".to_string(), pptr_value(file_id, path_id));
    tex_env.as_object_mut().unwrap().insert(
        "m_Offset".to_string(),
        UnityValue::Object(
            [
                ("x".to_string(), UnityValue::Float(0.0)),
                ("y".to_string(), UnityValue::Float(0.0)),
            ]
            .into_iter()
            .collect(),
        ),
    );
    tex_env.as_object_mut().unwrap().insert(
        "m_Scale".to_string(),
        UnityValue::Object(
            [
                ("x".to_string(), UnityValue::Float(1.0)),
                ("y".to_string(), UnityValue::Float(1.0)),
            ]
            .into_iter()
            .collect(),
        ),
    );

    let entry = UnityValue::Array(vec![UnityValue::String(property_name.to_string()), tex_env]);
    tex_envs.push(entry);

    Ok(())
}

pub(crate) fn apply_material_set_texenv_scale_offset(
    class: &mut UnityClass,
    property_name: &str,
    scale: (f64, f64),
    offset: (f64, f64),
) -> Result<()> {
    ensure_object_field(class, "m_SavedProperties");
    let Some(saved) = class.get_mut("m_SavedProperties") else {
        unreachable!();
    };

    let tex_envs = ensure_array_child(saved, "m_TexEnvs");

    for entry in tex_envs.iter_mut() {
        let Some((first, second)) = pair_first_second_mut(entry) else {
            continue;
        };
        let Some(name) = property_key_name(first) else {
            continue;
        };
        if name != property_name {
            continue;
        }

        let tex_env = ensure_object_child(second, "m_Offset");
        *tex_env = UnityValue::Object(
            [
                ("x".to_string(), UnityValue::Float(offset.0)),
                ("y".to_string(), UnityValue::Float(offset.1)),
            ]
            .into_iter()
            .collect(),
        );

        let tex_env = ensure_object_child(second, "m_Scale");
        *tex_env = UnityValue::Object(
            [
                ("x".to_string(), UnityValue::Float(scale.0)),
                ("y".to_string(), UnityValue::Float(scale.1)),
            ]
            .into_iter()
            .collect(),
        );

        return Ok(());
    }

    // Not found: append a new entry (string key variant).
    let tex_env = UnityValue::Object(
        [
            (
                "m_Offset".to_string(),
                UnityValue::Object(
                    [
                        ("x".to_string(), UnityValue::Float(offset.0)),
                        ("y".to_string(), UnityValue::Float(offset.1)),
                    ]
                    .into_iter()
                    .collect(),
                ),
            ),
            (
                "m_Scale".to_string(),
                UnityValue::Object(
                    [
                        ("x".to_string(), UnityValue::Float(scale.0)),
                        ("y".to_string(), UnityValue::Float(scale.1)),
                    ]
                    .into_iter()
                    .collect(),
                ),
            ),
            (
                "m_Texture".to_string(),
                UnityValue::Object(Default::default()),
            ),
        ]
        .into_iter()
        .collect(),
    );

    tex_envs.push(UnityValue::Array(vec![
        UnityValue::String(property_name.to_string()),
        tex_env,
    ]));

    Ok(())
}

pub(crate) fn apply_material_set_float(
    class: &mut UnityClass,
    property_name: &str,
    value: f64,
) -> Result<()> {
    ensure_object_field(class, "m_SavedProperties");
    let Some(saved) = class.get_mut("m_SavedProperties") else {
        unreachable!();
    };

    let floats = ensure_array_child(saved, "m_Floats");

    for entry in floats.iter_mut() {
        let Some((first, second)) = pair_first_second_mut(entry) else {
            continue;
        };
        let Some(name) = property_key_name(first) else {
            continue;
        };
        if name != property_name {
            continue;
        }
        *second = UnityValue::Float(value);
        return Ok(());
    }

    floats.push(UnityValue::Array(vec![
        UnityValue::String(property_name.to_string()),
        UnityValue::Float(value),
    ]));
    Ok(())
}

pub(crate) fn apply_material_set_int(
    class: &mut UnityClass,
    property_name: &str,
    value: i64,
) -> Result<()> {
    ensure_object_field(class, "m_SavedProperties");
    let Some(saved) = class.get_mut("m_SavedProperties") else {
        unreachable!();
    };

    let ints = ensure_array_child(saved, "m_Ints");

    for entry in ints.iter_mut() {
        let Some((first, second)) = pair_first_second_mut(entry) else {
            continue;
        };
        let Some(name) = property_key_name(first) else {
            continue;
        };
        if name != property_name {
            continue;
        }
        *second = UnityValue::Integer(value);
        return Ok(());
    }

    ints.push(UnityValue::Array(vec![
        UnityValue::String(property_name.to_string()),
        UnityValue::Integer(value),
    ]));
    Ok(())
}

pub(crate) fn apply_material_set_color(
    class: &mut UnityClass,
    property_name: &str,
    rgba: (f64, f64, f64, f64),
) -> Result<()> {
    ensure_object_field(class, "m_SavedProperties");
    let Some(saved) = class.get_mut("m_SavedProperties") else {
        unreachable!();
    };

    let colors = ensure_array_child(saved, "m_Colors");

    let color_value = UnityValue::Object(
        [
            ("r".to_string(), UnityValue::Float(rgba.0)),
            ("g".to_string(), UnityValue::Float(rgba.1)),
            ("b".to_string(), UnityValue::Float(rgba.2)),
            ("a".to_string(), UnityValue::Float(rgba.3)),
        ]
        .into_iter()
        .collect(),
    );

    for entry in colors.iter_mut() {
        let Some((first, second)) = pair_first_second_mut(entry) else {
            continue;
        };
        let Some(name) = property_key_name(first) else {
            continue;
        };
        if name != property_name {
            continue;
        }
        *second = color_value;
        return Ok(());
    }

    colors.push(UnityValue::Array(vec![
        UnityValue::String(property_name.to_string()),
        color_value,
    ]));
    Ok(())
}

pub(crate) fn apply_text_asset_script(class: &mut UnityClass, script: &str) -> Result<()> {
    let Some(v) = class.get_mut("m_Script") else {
        return Err(UnityAssetError::format(
            "TextAsset missing required field: m_Script",
        ));
    };
    *v = UnityValue::String(script.to_string());
    Ok(())
}

pub(crate) fn apply_mesh_streaming_write(
    class: &mut UnityClass,
    write: &StreamedResourceWrite,
) -> Result<()> {
    ensure_object_field(class, "m_StreamData");
    super::streamed_write::apply_streamed_resource_write(class, "m_StreamData", write)?;

    clear_bytes_field(class, "m_IndexBuffer");
    if let Some(v) = class.get_mut("m_VertexData") {
        clear_mesh_vertex_data_fields(v);
    }

    Ok(())
}

pub(crate) fn apply_video_clip_external_resources_write(
    class: &mut UnityClass,
    write: &StreamedResourceWrite,
) -> Result<()> {
    ensure_object_field(class, "m_ExternalResources");
    super::streamed_write::apply_streamed_resource_write(class, "m_ExternalResources", write)?;
    Ok(())
}

pub(crate) fn apply_video_player_url(class: &mut UnityClass, url: &str) -> Result<()> {
    let Some(v) = class.get_mut("m_Url") else {
        return Err(UnityAssetError::format(
            "VideoPlayer missing required field: m_Url",
        ));
    };
    *v = UnityValue::String(url.to_string());
    Ok(())
}

pub(crate) fn apply_video_player_video_clip_pptr(
    class: &mut UnityClass,
    file_id: i32,
    path_id: i64,
) -> Result<()> {
    apply_pptr_field(class, "m_VideoClip", file_id, path_id);
    Ok(())
}

pub(crate) fn apply_mesh_filter_mesh_pptr(class: &mut UnityClass, file_id: i32, path_id: i64) {
    apply_pptr_field(class, "m_Mesh", file_id, path_id);
}

pub(crate) fn apply_game_object_name(class: &mut UnityClass, name: &str) -> Result<()> {
    // Unity uses `m_Name` for GameObjects; some custom TypeTrees may expose `name`.
    let key = if class.get("m_Name").is_some() {
        "m_Name"
    } else if class.get("name").is_some() {
        "name"
    } else {
        "m_Name"
    };
    class.set(key.to_string(), UnityValue::String(name.to_string()));
    Ok(())
}

pub(crate) fn apply_game_object_active(class: &mut UnityClass, active: bool) -> Result<()> {
    let key = if class.get("m_IsActive").is_some() {
        "m_IsActive"
    } else if class.get("m_isActive").is_some() {
        "m_isActive"
    } else {
        "m_IsActive"
    };
    class.set(key.to_string(), UnityValue::Bool(active));
    Ok(())
}

pub(crate) fn apply_transform_local_position(
    class: &mut UnityClass,
    position: (f64, f64, f64),
) -> Result<()> {
    class.set(
        "m_LocalPosition".to_string(),
        vec3_value(position.0, position.1, position.2),
    );
    Ok(())
}

pub(crate) fn apply_transform_local_rotation(
    class: &mut UnityClass,
    rotation: (f64, f64, f64, f64),
) -> Result<()> {
    class.set(
        "m_LocalRotation".to_string(),
        quat_value(rotation.0, rotation.1, rotation.2, rotation.3),
    );
    Ok(())
}

pub(crate) fn apply_transform_local_scale(
    class: &mut UnityClass,
    scale: (f64, f64, f64),
) -> Result<()> {
    class.set(
        "m_LocalScale".to_string(),
        vec3_value(scale.0, scale.1, scale.2),
    );
    Ok(())
}

pub(crate) fn apply_rect_transform_anchored_position(
    class: &mut UnityClass,
    position: (f64, f64),
) -> Result<()> {
    // Some layouts use `m_AnchoredPosition3D`; prefer the 2D field when present.
    if class.get("m_AnchoredPosition").is_some() {
        class.set(
            "m_AnchoredPosition".to_string(),
            vec2_value(position.0, position.1),
        );
    } else if class.get("m_AnchoredPosition3D").is_some() {
        class.set(
            "m_AnchoredPosition3D".to_string(),
            vec3_value(position.0, position.1, 0.0),
        );
    } else {
        class.set(
            "m_AnchoredPosition".to_string(),
            vec2_value(position.0, position.1),
        );
    }
    Ok(())
}

pub(crate) fn apply_rect_transform_size_delta(
    class: &mut UnityClass,
    size: (f64, f64),
) -> Result<()> {
    class.set("m_SizeDelta".to_string(), vec2_value(size.0, size.1));
    Ok(())
}

pub(crate) fn apply_rect_transform_anchor_min(class: &mut UnityClass, v: (f64, f64)) -> Result<()> {
    class.set("m_AnchorMin".to_string(), vec2_value(v.0, v.1));
    Ok(())
}

pub(crate) fn apply_rect_transform_anchor_max(class: &mut UnityClass, v: (f64, f64)) -> Result<()> {
    class.set("m_AnchorMax".to_string(), vec2_value(v.0, v.1));
    Ok(())
}

pub(crate) fn apply_rect_transform_pivot(class: &mut UnityClass, v: (f64, f64)) -> Result<()> {
    class.set("m_Pivot".to_string(), vec2_value(v.0, v.1));
    Ok(())
}

pub(crate) fn apply_rect_transform_offset_min(class: &mut UnityClass, v: (f64, f64)) -> Result<()> {
    class.set("m_OffsetMin".to_string(), vec2_value(v.0, v.1));
    Ok(())
}

pub(crate) fn apply_rect_transform_offset_max(class: &mut UnityClass, v: (f64, f64)) -> Result<()> {
    class.set("m_OffsetMax".to_string(), vec2_value(v.0, v.1));
    Ok(())
}

pub(crate) fn apply_renderer_materials(
    class: &mut UnityClass,
    materials: &[(i32, i64)],
) -> Result<()> {
    class.set(
        "m_Materials".to_string(),
        UnityValue::Array(
            materials
                .iter()
                .copied()
                .map(|(file_id, path_id)| pptr_value(file_id, path_id))
                .collect(),
        ),
    );
    Ok(())
}

pub(crate) fn apply_mesh_renderer_additional_vertex_streams_pptr(
    class: &mut UnityClass,
    file_id: i32,
    path_id: i64,
) -> Result<()> {
    apply_pptr_field(class, "m_AdditionalVertexStreams", file_id, path_id);
    Ok(())
}

impl<'a> EnvironmentEditSession<'a> {
    /// Write `data` into a cab and configure an AudioClip to stream from it (UnityPy-style).
    ///
    /// This updates `m_Resource` when present (preferred by UnityPy), falling back to `m_StreamData`
    /// when needed, and clears `m_AudioData` to avoid embedding bytes.
    pub fn write_streamed_audio_clip_data(
        &mut self,
        key: &BinaryObjectKey,
        cab_name: Option<&str>,
        data: &[u8],
    ) -> Result<StreamedResourceWrite> {
        let write = match self.write_streamed_resource_to_field(key, "m_Resource", cab_name, data) {
            Ok(write) => write,
            Err(err_primary) => self
                .write_streamed_resource_to_field(key, "m_StreamData", cab_name, data)
                .map_err(|err_fallback| {
                    unity_asset_core::UnityAssetError::format(format!(
                        "Failed to update AudioClip stream field: m_Resource={}; m_StreamData={}",
                        err_primary, err_fallback
                    ))
                })?,
        };

        self.edit_binary_object_key(key, |class| {
            if let Some(v) = class.get_mut("m_AudioData") {
                *v = UnityValue::Bytes(Vec::new());
            }
            Ok(())
        })?;

        Ok(write)
    }

    /// Write `data` into a cab and configure a Texture2D to stream from it (UnityPy-style).
    ///
    /// This updates `m_StreamData` and clears embedded image byte fields if present.
    pub fn write_streamed_texture2d_image_data(
        &mut self,
        key: &BinaryObjectKey,
        cab_name: Option<&str>,
        data: &[u8],
    ) -> Result<StreamedResourceWrite> {
        let write = self.write_streamed_resource_to_field(key, "m_StreamData", cab_name, data)?;

        let len_i64: i64 = data.len().try_into().unwrap_or(i64::MAX);
        self.edit_binary_object_key(key, |class| {
            for name in ["image_data", "image data", "m_ImageData"] {
                clear_bytes_field(class, name);
            }

            if let Some(v) = class.get_mut("m_CompleteImageSize") {
                *v = UnityValue::Integer(len_i64);
            }
            if let Some(v) = class.get_mut("m_DataSize") {
                *v = UnityValue::Integer(len_i64);
            }

            Ok(())
        })?;

        Ok(write)
    }

    /// Write `data` into a cab and configure a Mesh to stream from it (UnityPy-style).
    ///
    /// This updates `m_StreamData` and clears common embedded buffers when present.
    pub fn write_streamed_mesh_data(
        &mut self,
        key: &BinaryObjectKey,
        cab_name: Option<&str>,
        data: &[u8],
    ) -> Result<StreamedResourceWrite> {
        let write = self.write_to_cab(key, cab_name, data)?;
        self.edit_binary_object_key(key, |class| apply_mesh_streaming_write(class, &write))?;
        Ok(write)
    }

    /// Write `data` into a cab and configure a VideoClip to stream from it (UnityPy-style).
    ///
    /// UnityPy reads this via `m_ExternalResources: { m_Source, m_Offset, m_Size }`.
    pub fn write_streamed_video_clip_data(
        &mut self,
        key: &BinaryObjectKey,
        cab_name: Option<&str>,
        data: &[u8],
    ) -> Result<StreamedResourceWrite> {
        let write = self.write_to_cab(key, cab_name, data)?;
        self.edit_binary_object_key(key, |class| {
            apply_video_clip_external_resources_write(class, &write)
        })?;
        Ok(write)
    }

    /// Set the `m_Script` string on a TextAsset (UnityPy-like convenience helper).
    pub fn set_text_asset_script(&mut self, key: &BinaryObjectKey, script: &str) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_text_asset_script(class, script))
    }

    /// Set the `m_Url` string on a VideoPlayer (UnityPy-like convenience helper).
    pub fn set_video_player_url(&mut self, key: &BinaryObjectKey, url: &str) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_video_player_url(class, url))
    }

    /// Set the `m_VideoClip` PPtr on a VideoPlayer (UnityPy-like convenience helper).
    ///
    /// Notes:
    /// - Use `file_id=0` for a VideoClip inside the same serialized file as the VideoPlayer.
    /// - External references require the correct `file_id` index into the file's `externals` table.
    pub fn set_video_player_video_clip_pptr(
        &mut self,
        key: &BinaryObjectKey,
        file_id: i32,
        path_id: i64,
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| {
            apply_video_player_video_clip_pptr(class, file_id, path_id)
        })
    }

    /// Set a Unity `PPtr`-shaped field (`fileID/pathID`) in a best-effort manner.
    ///
    /// This supports both `fileID/pathID` and `m_FileID/m_PathID` key variants and will create the
    /// field object if needed.
    pub fn set_pptr_field(
        &mut self,
        key: &BinaryObjectKey,
        field_name: &str,
        file_id: i32,
        path_id: i64,
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| {
            apply_pptr_field(class, field_name, file_id, path_id);
            Ok(())
        })
    }

    /// Set the `m_Mesh` PPtr on a MeshFilter (UnityPy-like convenience helper).
    pub fn set_mesh_filter_mesh_pptr(
        &mut self,
        key: &BinaryObjectKey,
        file_id: i32,
        path_id: i64,
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| {
            apply_mesh_filter_mesh_pptr(class, file_id, path_id);
            Ok(())
        })
    }

    /// Replace the `m_Materials` list on any `Renderer`-like object with the provided PPtr list.
    ///
    /// This is intentionally tolerant and only requires the typetree to accept `m_Materials`.
    pub fn set_renderer_materials(
        &mut self,
        key: &BinaryObjectKey,
        materials: &[(i32, i64)],
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_renderer_materials(class, materials))
    }

    /// Replace the `m_Materials` list on any `Renderer`-like object using a list of object keys.
    ///
    /// This computes `fileID` values automatically and appends externals as needed.
    pub fn set_renderer_materials_to_keys(
        &mut self,
        renderer_key: &BinaryObjectKey,
        material_keys: &[BinaryObjectKey],
    ) -> Result<()> {
        let mut materials: Vec<(i32, i64)> = Vec::with_capacity(material_keys.len());
        for material_key in material_keys {
            let file_id = self.file_id_for_target(renderer_key, material_key)?;
            materials.push((file_id, material_key.path_id));
        }
        self.set_renderer_materials(renderer_key, &materials)
    }

    /// Replace the `m_Materials` list on a MeshRenderer with the provided PPtr list.
    pub fn set_mesh_renderer_materials(
        &mut self,
        key: &BinaryObjectKey,
        materials: &[(i32, i64)],
    ) -> Result<()> {
        self.set_renderer_materials(key, materials)
    }

    /// Replace the `m_Materials` list on a MeshRenderer using a list of object keys.
    pub fn set_mesh_renderer_materials_to_keys(
        &mut self,
        mesh_renderer_key: &BinaryObjectKey,
        material_keys: &[BinaryObjectKey],
    ) -> Result<()> {
        self.set_renderer_materials_to_keys(mesh_renderer_key, material_keys)
    }

    /// Set `m_AdditionalVertexStreams` on a MeshRenderer (best-effort; optional field in Unity).
    pub fn set_mesh_renderer_additional_vertex_streams_pptr(
        &mut self,
        key: &BinaryObjectKey,
        file_id: i32,
        path_id: i64,
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| {
            apply_mesh_renderer_additional_vertex_streams_pptr(class, file_id, path_id)
        })
    }

    /// Set `m_AdditionalVertexStreams` on a MeshRenderer using an object key.
    pub fn set_mesh_renderer_additional_vertex_streams_to_key(
        &mut self,
        mesh_renderer_key: &BinaryObjectKey,
        mesh_key: &BinaryObjectKey,
    ) -> Result<()> {
        let file_id = self.file_id_for_target(mesh_renderer_key, mesh_key)?;
        self.set_mesh_renderer_additional_vertex_streams_pptr(
            mesh_renderer_key,
            file_id,
            mesh_key.path_id,
        )
    }

    /// Set a Material `m_SavedProperties.m_TexEnvs[*].m_Texture` entry by property name.
    ///
    /// This is the most common workflow for repointing textures (e.g. `_MainTex`).
    pub fn set_material_texenv_texture_to_key(
        &mut self,
        material_key: &BinaryObjectKey,
        property_name: &str,
        texture_key: &BinaryObjectKey,
    ) -> Result<()> {
        let file_id = self.file_id_for_target(material_key, texture_key)?;
        self.edit_binary_object_key(material_key, |class| {
            apply_material_set_texenv_texture_pptr(
                class,
                property_name,
                file_id,
                texture_key.path_id,
            )
        })
    }

    pub fn set_material_texenv_scale_offset(
        &mut self,
        material_key: &BinaryObjectKey,
        property_name: &str,
        scale: (f64, f64),
        offset: (f64, f64),
    ) -> Result<()> {
        self.edit_binary_object_key(material_key, |class| {
            apply_material_set_texenv_scale_offset(class, property_name, scale, offset)
        })
    }

    pub fn set_material_float(
        &mut self,
        material_key: &BinaryObjectKey,
        property_name: &str,
        value: f64,
    ) -> Result<()> {
        self.edit_binary_object_key(material_key, |class| {
            apply_material_set_float(class, property_name, value)
        })
    }

    pub fn set_material_int(
        &mut self,
        material_key: &BinaryObjectKey,
        property_name: &str,
        value: i64,
    ) -> Result<()> {
        self.edit_binary_object_key(material_key, |class| {
            apply_material_set_int(class, property_name, value)
        })
    }

    pub fn set_material_color_rgba(
        &mut self,
        material_key: &BinaryObjectKey,
        property_name: &str,
        rgba: (f64, f64, f64, f64),
    ) -> Result<()> {
        self.edit_binary_object_key(material_key, |class| {
            apply_material_set_color(class, property_name, rgba)
        })
    }

    pub fn set_game_object_name(&mut self, key: &BinaryObjectKey, name: &str) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_game_object_name(class, name))
    }

    pub fn set_game_object_active(&mut self, key: &BinaryObjectKey, active: bool) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_game_object_active(class, active))
    }

    pub fn set_transform_local_position(
        &mut self,
        key: &BinaryObjectKey,
        position: (f64, f64, f64),
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_transform_local_position(class, position))
    }

    pub fn set_transform_local_rotation(
        &mut self,
        key: &BinaryObjectKey,
        rotation: (f64, f64, f64, f64),
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_transform_local_rotation(class, rotation))
    }

    pub fn set_transform_local_scale(
        &mut self,
        key: &BinaryObjectKey,
        scale: (f64, f64, f64),
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_transform_local_scale(class, scale))
    }

    pub fn set_rect_transform_anchored_position(
        &mut self,
        key: &BinaryObjectKey,
        position: (f64, f64),
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| {
            apply_rect_transform_anchored_position(class, position)
        })
    }

    pub fn set_rect_transform_size_delta(
        &mut self,
        key: &BinaryObjectKey,
        size: (f64, f64),
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_rect_transform_size_delta(class, size))
    }

    pub fn set_rect_transform_anchor_min(
        &mut self,
        key: &BinaryObjectKey,
        v: (f64, f64),
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_rect_transform_anchor_min(class, v))
    }

    pub fn set_rect_transform_anchor_max(
        &mut self,
        key: &BinaryObjectKey,
        v: (f64, f64),
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_rect_transform_anchor_max(class, v))
    }

    pub fn set_rect_transform_pivot(&mut self, key: &BinaryObjectKey, v: (f64, f64)) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_rect_transform_pivot(class, v))
    }

    pub fn set_rect_transform_offset_min(
        &mut self,
        key: &BinaryObjectKey,
        v: (f64, f64),
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_rect_transform_offset_min(class, v))
    }

    pub fn set_rect_transform_offset_max(
        &mut self,
        key: &BinaryObjectKey,
        v: (f64, f64),
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_rect_transform_offset_max(class, v))
    }
}
