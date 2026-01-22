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

fn ensure_pptr_field(class: &mut UnityClass, field_name: &str) {
    ensure_object_field(class, field_name);
    match class.get_mut(field_name) {
        Some(other) => {
            *other = UnityValue::Object(Default::default());
        }
        None => {}
    }
}

fn set_pptr_object_fields(obj: &mut UnityValue, file_id: i32, path_id: i64) {
    let Some(map) = obj.as_object_mut() else {
        return;
    };

    let file_id_value = UnityValue::Integer(file_id as i64);
    let path_id_value = UnityValue::Integer(path_id);

    // Support both common PPtr key variants.
    for key in ["fileID", "m_FileID"] {
        map.insert(key.to_string(), file_id_value.clone());
    }
    for key in ["pathID", "m_PathID"] {
        map.insert(key.to_string(), path_id_value.clone());
    }
}

fn apply_pptr_field(class: &mut UnityClass, field_name: &str, file_id: i32, path_id: i64) {
    ensure_pptr_field(class, field_name);
    if let Some(v) = class.get_mut(field_name) {
        set_pptr_object_fields(v, file_id, path_id);
    }
}

fn pptr_value(file_id: i32, path_id: i64) -> UnityValue {
    let mut v = UnityValue::Object(Default::default());
    set_pptr_object_fields(&mut v, file_id, path_id);
    v
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

pub(crate) fn apply_mesh_renderer_materials(
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

    /// Replace the `m_Materials` list on a MeshRenderer with the provided PPtr list.
    pub fn set_mesh_renderer_materials(
        &mut self,
        key: &BinaryObjectKey,
        materials: &[(i32, i64)],
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| apply_mesh_renderer_materials(class, materials))
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
}
