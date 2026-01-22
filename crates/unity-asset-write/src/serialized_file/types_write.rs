use crate::Result;
use crate::binary_writer::BinaryWriter;
use crate::serialized_file::typetree_dump::{dump_typetree_blob, dump_typetree_legacy};
use unity_asset_binary::asset::types::LocalSerializedObjectIdentifier;
use unity_asset_binary::asset::{FileIdentifier, SerializedType};
use unity_asset_core::UnityAssetError;

pub fn write_file_identifier(
    v: &FileIdentifier,
    writer: &mut BinaryWriter,
    version: u32,
) -> Result<()> {
    if version >= 6 {
        writer.write_string_to_null(&v.temp_empty);
    }
    if version >= 5 {
        writer.write(v.guid.as_slice());
        writer.write_i32(v.type_);
    }
    writer.write_string_to_null(&v.path);
    Ok(())
}

pub fn write_local_serialized_object_identifier(
    v: &LocalSerializedObjectIdentifier,
    writer: &mut BinaryWriter,
    version: u32,
) -> Result<()> {
    writer.write_i32(v.local_serialized_file_index);
    if version < 14 {
        let id_i32: i32 = v.local_identifier_in_file.try_into().map_err(|_| {
            UnityAssetError::format(format!(
                "local_identifier_in_file does not fit i32: {}",
                v.local_identifier_in_file
            ))
        })?;
        writer.write_i32(id_i32);
    } else {
        writer.align_stream(4);
        writer.write_i64(v.local_identifier_in_file);
    }
    Ok(())
}

pub fn write_serialized_type(
    st: &SerializedType,
    writer: &mut BinaryWriter,
    file_version: u32,
    enable_type_tree: bool,
    is_ref_type: bool,
) -> Result<()> {
    writer.write_i32(st.class_id);

    if file_version >= 16 {
        writer.write_bool(st.is_stripped_type);
    }

    if file_version >= 17 {
        writer.write_i16(st.script_type_index);
    }

    if file_version >= 13 {
        let should_write_script_id = (is_ref_type && st.script_type_index >= 0)
            || (file_version < 16 && st.class_id < 0)
            || (file_version >= 16 && st.class_id == 114); // MonoBehaviour

        if should_write_script_id {
            writer.write(st.script_id.as_slice());
        }
        writer.write(st.old_type_hash.as_slice());
    }

    if enable_type_tree {
        if file_version >= 12 || file_version == 10 {
            dump_typetree_blob(&st.type_tree, writer, file_version)?;
        } else {
            dump_typetree_legacy(&st.type_tree, writer, file_version)?;
        }

        if file_version >= 21 {
            if is_ref_type {
                writer.write_string_to_null(&st.class_name);
                writer.write_string_to_null(&st.namespace);
                writer.write_string_to_null(&st.assembly_name);
            } else {
                let count_i32: i32 = st.type_dependencies.len().try_into().map_err(|_| {
                    UnityAssetError::format(format!(
                        "type_dependencies too large: {}",
                        st.type_dependencies.len()
                    ))
                })?;
                writer.write_i32(count_i32);
                for dep in &st.type_dependencies {
                    writer.write_i32(*dep);
                }
            }
        }
    }

    Ok(())
}
