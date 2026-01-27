use indexmap::IndexMap;
use unity_asset_binary::webfile::{WebFile, WebFileCompression};
use unity_asset_core::{Result, UnityAssetError};

use crate::compression::{compress_brotli, compress_gzip};
use crate::{BinaryWriter, Endian};

use super::WebFileEdits;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebFilePacker {
    None,
    Gzip,
    Brotli,
    Original,
}

pub struct WebFileWriter;

impl WebFileWriter {
    pub fn save(
        web: &WebFile,
        edits: &WebFileEdits,
        packer: WebFilePacker,
        signature: Option<&str>,
    ) -> Result<Vec<u8>> {
        let signature = signature.unwrap_or("UnityWebData1.0");
        if !signature.starts_with("UnityWebData") && !signature.starts_with("TuanjieWebData") {
            return Err(UnityAssetError::format(format!(
                "Invalid WebFile signature: {signature:?}. Expected 'UnityWebData*' or 'TuanjieWebData*'."
            )));
        }

        // UnityPy stores files in a dict (insertion order preserved; assignments update value).
        // Using IndexMap gives the same semantics: first insert determines order, later inserts replace.
        let mut files: IndexMap<String, Vec<u8>> = IndexMap::new();

        for info in web.files() {
            let bytes = if let Some(replacement) = edits.get(&info.name) {
                replacement.to_vec()
            } else {
                web.extract_file_slice_by_info(info)
                    .map_err(|e| {
                        UnityAssetError::with_source(
                            format!("Failed to extract WebFile entry bytes: {}", info.name),
                            e,
                        )
                    })?
                    .to_vec()
            };
            files.insert(info.name.clone(), bytes);
        }

        // Apply any extra edits as new entries (or replacements that keep order).
        for (name, bytes) in edits.iter() {
            files.insert(name.to_string(), bytes.to_vec());
        }

        // Write uncompressed payload.
        let mut writer = BinaryWriter::new(Endian::Little);
        writer.write_string_to_null(signature);

        let total_path_bytes: usize = files.keys().map(|k| k.len()).sum();
        let entry_table_bytes = 12usize
            .checked_mul(files.len())
            .ok_or_else(|| UnityAssetError::format("WebFile entry table size overflow"))?;

        let offset = writer
            .position()
            .checked_add(total_path_bytes)
            .and_then(|v| v.checked_add(entry_table_bytes))
            .and_then(|v| v.checked_add(4))
            .ok_or_else(|| UnityAssetError::format("WebFile header offset overflow"))?;

        let offset_i32: i32 = offset.try_into().map_err(|_| {
            UnityAssetError::format(format!(
                "WebFile header offset too large for i32: {}",
                offset
            ))
        })?;
        writer.write_i32(offset_i32);

        // 1) file headers
        let mut cursor = offset;
        for (name, data) in files.iter() {
            let cursor_i32: i32 = cursor.try_into().map_err(|_| {
                UnityAssetError::format(format!(
                    "WebFile entry offset too large for i32: {}",
                    cursor
                ))
            })?;
            let len_i32: i32 = data.len().try_into().map_err(|_| {
                UnityAssetError::format(format!(
                    "WebFile entry too large for i32 length: {}",
                    data.len()
                ))
            })?;
            let path_bytes = name.as_bytes();
            let path_len_i32: i32 = path_bytes.len().try_into().map_err(|_| {
                UnityAssetError::format(format!(
                    "WebFile entry path too large for i32 length: {}",
                    path_bytes.len()
                ))
            })?;

            writer.write_i32(cursor_i32);
            writer.write_i32(len_i32);
            writer.write_i32(path_len_i32);
            writer.write(path_bytes);

            cursor = cursor
                .checked_add(data.len())
                .ok_or_else(|| UnityAssetError::format("WebFile data cursor overflow"))?;
        }

        // 2) file data
        for data in files.values() {
            writer.write(data);
        }

        let payload = writer.into_bytes();

        let resolved = match packer {
            WebFilePacker::Original => match web.compression {
                WebFileCompression::None => WebFilePacker::None,
                WebFileCompression::Gzip => WebFilePacker::Gzip,
                WebFileCompression::Brotli => WebFilePacker::Brotli,
            },
            other => other,
        };

        Ok(match resolved {
            WebFilePacker::None => payload,
            WebFilePacker::Gzip => compress_gzip(&payload),
            WebFilePacker::Brotli => compress_brotli(&payload),
            WebFilePacker::Original => unreachable!("resolved above"),
        })
    }
}
