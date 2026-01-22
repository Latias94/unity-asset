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
}
