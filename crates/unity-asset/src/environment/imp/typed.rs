use super::edit::{EnvironmentEditSession, StreamedResourceWrite};
use super::{BinaryObjectKey, Result};
use unity_asset_core::UnityValue;

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
                if let Some(v) = class.get_mut(name) {
                    *v = UnityValue::Bytes(Vec::new());
                }
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
}
