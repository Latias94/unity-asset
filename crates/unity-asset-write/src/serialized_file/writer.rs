use crate::Result;
use crate::binary_writer::{BinaryWriter, Endian};
use crate::serialized_file::edit::SerializedFileEdits;
use crate::serialized_file::types_write::{
    write_file_identifier, write_local_serialized_object_identifier, write_serialized_type,
};
use unity_asset_binary::asset::{ObjectInfo, SerializedFile};
use unity_asset_core::UnityAssetError;

#[derive(Debug, Clone, Copy)]
pub struct SerializedFileSaveOptions {
    /// Best-effort: allow saving even if not all object bytes were preloaded.
    ///
    /// When false, saving requires `ObjectInfo.data` to be present for all objects.
    pub allow_lazy_object_reads: bool,
}

impl Default for SerializedFileSaveOptions {
    fn default() -> Self {
        Self {
            allow_lazy_object_reads: true,
        }
    }
}

pub struct SerializedFileWriter;

impl SerializedFileWriter {
    pub fn save(file: &SerializedFile, edits: &SerializedFileEdits) -> Result<Vec<u8>> {
        Self::save_with_options(file, edits, SerializedFileSaveOptions::default())
    }

    pub fn save_with_options(
        file: &SerializedFile,
        edits: &SerializedFileEdits,
        options: SerializedFileSaveOptions,
    ) -> Result<Vec<u8>> {
        let version = file.header.version;
        if version < 9 {
            return Err(UnityAssetError::format(
                "SerializedFile save for version < 9 is not implemented yet",
            ));
        }

        let endian = match file.header.byte_order() {
            unity_asset_binary::reader::ByteOrder::Little => Endian::Little,
            unity_asset_binary::reader::ByteOrder::Big => Endian::Big,
        };

        let mut meta = BinaryWriter::new(endian);
        let mut data = BinaryWriter::new(endian);

        // Unity version string (v>=7)
        if version >= 7 {
            meta.write_string_to_null(&file.unity_version);
        }

        // Target platform (v>=8)
        if version >= 8 {
            meta.write_i32(file.target_platform);
        }

        // enableTypeTree (v>=13)
        if version >= 13 {
            meta.write_bool(file.enable_type_tree);
        }

        // Types
        let type_count_i32: i32 = file.types.len().try_into().map_err(|_| {
            UnityAssetError::format(format!("type count too large: {}", file.types.len()))
        })?;
        meta.write_i32(type_count_i32);
        for st in &file.types {
            write_serialized_type(st, &mut meta, version, file.enable_type_tree, false)?;
        }

        // bigIdEnabled (7<=v<14)
        if (7..14).contains(&version) {
            meta.write_i32(if file.big_id_enabled { 1 } else { 0 });
        }

        // Objects: table in metadata, payloads in data stream.
        let obj_count_i32: i32 = file.objects.len().try_into().map_err(|_| {
            UnityAssetError::format(format!("object count too large: {}", file.objects.len()))
        })?;
        meta.write_i32(obj_count_i32);

        for info in &file.objects {
            write_object_entry(file, info, edits, &mut meta, &mut data, options)?;
            // UnityPy aligns object data stream to 8 after each object.
            data.align_stream(8);
        }

        // Script types (v>=11)
        if version >= 11 {
            let script_count_i32: i32 = file.script_types.len().try_into().map_err(|_| {
                UnityAssetError::format(format!(
                    "script type count too large: {}",
                    file.script_types.len()
                ))
            })?;
            meta.write_i32(script_count_i32);
            for s in &file.script_types {
                write_local_serialized_object_identifier(s, &mut meta, version)?;
            }
        }

        // Externals
        let mut externals = file.externals.clone();
        for ext in &edits.additional_externals {
            if !externals.iter().any(|e| e.path == ext.path) {
                externals.push(ext.clone());
            }
        }

        let ext_count_i32: i32 = externals.len().try_into().map_err(|_| {
            UnityAssetError::format(format!("external count too large: {}", externals.len()))
        })?;
        meta.write_i32(ext_count_i32);
        for e in &externals {
            write_file_identifier(e, &mut meta, version)?;
        }

        // Ref types (v>=20)
        if version >= 20 {
            let ref_count_i32: i32 = file.ref_types.len().try_into().map_err(|_| {
                UnityAssetError::format(format!(
                    "ref type count too large: {}",
                    file.ref_types.len()
                ))
            })?;
            meta.write_i32(ref_count_i32);
            for st in &file.ref_types {
                write_serialized_type(st, &mut meta, version, file.enable_type_tree, true)?;
            }
        }

        // userInformation (v>=5)
        if version >= 5 {
            meta.write_string_to_null(&file.user_information);
        }

        // Header + layout
        let metadata_size = meta.len();
        let data_size = data.len();

        let header_size: usize = if version >= 22 { 48 } else { 20 };
        let mut data_offset = header_size + metadata_size;
        data_offset += (16 - (data_offset % 16)) % 16;
        let file_size = data_offset
            .checked_add(data_size)
            .ok_or_else(|| UnityAssetError::format("file size overflow"))?;

        let metadata_size_u32: u32 = metadata_size.try_into().map_err(|_| {
            UnityAssetError::format(format!("metadata_size does not fit u32: {}", metadata_size))
        })?;
        let file_size_u32: u32 = if version < 22 {
            file_size.try_into().map_err(|_| {
                UnityAssetError::format(format!("file_size does not fit u32: {}", file_size))
            })?
        } else {
            0
        };
        let data_offset_u32: u32 = if version < 22 {
            data_offset.try_into().map_err(|_| {
                UnityAssetError::format(format!("data_offset does not fit u32: {}", data_offset))
            })?
        } else {
            0
        };

        // Unity SerializedFile header fields are always written in big-endian order.
        // UnityPy uses `EndianBinaryWriter()` default (`">"`).
        let mut out = BinaryWriter::new(Endian::Big);
        if version < 22 {
            out.write_u32(metadata_size_u32);
            out.write_u32(file_size_u32);
            out.write_u32(version);
            out.write_u32(data_offset_u32);
            out.write_bool(file.header.endian != 0);
            out.write(&file.header.reserved);

            out.write(meta.bytes());
            out.align_stream(16);
            out.write(data.bytes());
        } else {
            // UnityPy writes an "old" header with zeros, followed by the extended fields.
            out.write_u32(0);
            out.write_u32(0);
            out.write_u32(version);
            out.write_u32(0);
            out.write_bool(file.header.endian != 0);
            out.write(&file.header.reserved);
            out.write_u32(metadata_size_u32);
            out.write_i64(file_size as i64);
            out.write_i64(data_offset as i64);
            out.write_i64(file.header.unknown);

            out.write(meta.bytes());
            out.align_stream(16);
            out.write(data.bytes());
        }

        Ok(out.into_bytes())
    }
}

fn write_object_entry(
    file: &SerializedFile,
    info: &ObjectInfo,
    edits: &SerializedFileEdits,
    meta: &mut BinaryWriter,
    data: &mut BinaryWriter,
    options: SerializedFileSaveOptions,
) -> Result<()> {
    let version = file.header.version;

    // Path ID
    if file.big_id_enabled {
        meta.write_i64(info.path_id);
    } else if version < 14 {
        let pid_i32: i32 = info.path_id.try_into().map_err(|_| {
            UnityAssetError::format(format!("path_id does not fit i32: {}", info.path_id))
        })?;
        meta.write_i32(pid_i32);
    } else {
        meta.align_stream(4);
        meta.write_i64(info.path_id);
    }

    // Object bytes (override -> inline -> slice)
    let obj_bytes: Vec<u8> = if let Some(override_bytes) = edits.get(info.path_id) {
        override_bytes.to_vec()
    } else if !info.data.is_empty() {
        info.data.clone()
    } else if options.allow_lazy_object_reads {
        file.object_bytes(info)
            .map_err(|e| UnityAssetError::with_source("Failed to read object bytes", e))?
            .to_vec()
    } else {
        return Err(UnityAssetError::format(format!(
            "Object {} bytes not loaded (path_id={})",
            info.type_id, info.path_id
        )));
    };

    // Byte start (relative to data stream, NOT including header.data_offset)
    if version >= 22 {
        meta.write_i64(data.position() as i64);
    } else {
        let pos_u32: u32 = data.position().try_into().map_err(|_| {
            UnityAssetError::format(format!(
                "data stream position does not fit u32: {}",
                data.position()
            ))
        })?;
        meta.write_u32(pos_u32);
    }

    // Byte size
    meta.write_u32(obj_bytes.len() as u32);

    // Type ID / type index in object table
    let raw_type_id = if version >= 16 && info.type_index >= 0 {
        info.type_index
    } else {
        info.type_id
    };
    meta.write_i32(raw_type_id);

    if version < 16 {
        meta.write_u16(info.type_id as u16);
    }

    if version < 11 {
        meta.write_u16(0);
    }

    if (11..17).contains(&version) {
        let script_type_index = if version < 16 {
            file.types
                .iter()
                .find(|t| t.class_id == raw_type_id)
                .map(|t| t.script_type_index)
                .unwrap_or(-1)
        } else {
            file.types
                .get(raw_type_id as usize)
                .map(|t| t.script_type_index)
                .unwrap_or(-1)
        };
        meta.write_i16(script_type_index);
    }

    if version == 15 || version == 16 {
        meta.write_u8(0);
    }

    data.write(obj_bytes.as_slice());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_save_serialized_file_extracted_from_bundle_and_reload() {
        // Use an existing UnityFS sample bundle and pick its first SerializedFile.
        let bundle_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/samples/char_118_yuki.ab");
        let bundle_bytes = std::fs::read(bundle_path).unwrap();
        let bundle = unity_asset_binary::bundle::load_bundle_from_memory(bundle_bytes).unwrap();
        let sf = bundle.assets.first().expect("bundle has assets");

        let out = SerializedFileWriter::save(sf, &SerializedFileEdits::new()).unwrap();
        let reparsed = unity_asset_binary::asset::SerializedFileParser::from_bytes(out).unwrap();

        assert_eq!(reparsed.header.version, sf.header.version);
        assert_eq!(reparsed.unity_version, sf.unity_version);
        assert_eq!(reparsed.target_platform, sf.target_platform);
        assert_eq!(reparsed.enable_type_tree, sf.enable_type_tree);
        assert_eq!(reparsed.types.len(), sf.types.len());
        assert_eq!(reparsed.objects.len(), sf.objects.len());
        assert_eq!(reparsed.externals.len(), sf.externals.len());
        assert_eq!(reparsed.ref_types.len(), sf.ref_types.len());
    }
}
