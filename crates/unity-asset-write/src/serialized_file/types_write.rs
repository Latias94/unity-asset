use crate::Result;
use crate::serialized_file::sink::{EndianSink, SinkBackend};
use crate::serialized_file::typetree_dump::dump_typetree_to;
use unity_asset_binary::asset::types::LocalSerializedObjectIdentifier;
use unity_asset_binary::asset::{
    ExternalEncoding, FileIdentifier, PathIdEncoding, SerializedFileFormat, SerializedType,
};
use unity_asset_binary::typetree::TypeTreeParser;
use unity_asset_core::UnityAssetError;

pub(crate) fn write_file_identifier_to<B: SinkBackend>(
    v: &FileIdentifier,
    writer: &mut EndianSink<B>,
    format: SerializedFileFormat,
) -> Result<()> {
    match format.external_encoding() {
        ExternalEncoding::PathOnly => {
            if !v.temp_empty.is_empty() || v.guid != [0; 16] || v.type_ != 0 {
                return Err(UnityAssetError::format(format!(
                    "SerializedFile v{} cannot encode GUID, type, or asset-path external metadata",
                    format.version()
                )));
            }
        }
        ExternalEncoding::GuidAndType => {
            if !v.temp_empty.is_empty() {
                return Err(UnityAssetError::format(format!(
                    "SerializedFile v{} cannot encode an external asset path",
                    format.version()
                )));
            }
            writer.write(v.guid.as_slice())?;
            writer.write_i32(v.type_)?;
        }
        ExternalEncoding::AssetPathGuidAndType => {
            writer.write_string_to_null(&v.temp_empty)?;
            writer.write(v.guid.as_slice())?;
            writer.write_i32(v.type_)?;
        }
    }
    writer.write_string_to_null(&v.path)
}

pub(crate) fn write_local_serialized_object_identifier_to<B: SinkBackend>(
    v: &LocalSerializedObjectIdentifier,
    writer: &mut EndianSink<B>,
    format: SerializedFileFormat,
) -> Result<()> {
    writer.write_i32(v.local_serialized_file_index)?;
    match format.path_id_encoding() {
        PathIdEncoding::I32 | PathIdEncoding::BigIdFlag => {
            let id = i32::try_from(v.local_identifier_in_file).map_err(|_| {
                UnityAssetError::format(format!(
                    "local_identifier_in_file does not fit i32: {}",
                    v.local_identifier_in_file
                ))
            })?;
            writer.write_i32(id)?;
        }
        PathIdEncoding::AlignedI64 => {
            writer.align_stream(4)?;
            writer.write_i64(v.local_identifier_in_file)?;
        }
    }
    Ok(())
}

pub(crate) fn write_serialized_type_to<B: SinkBackend>(
    st: &SerializedType,
    writer: &mut EndianSink<B>,
    format: SerializedFileFormat,
    enable_type_tree: bool,
    is_ref_type: bool,
) -> Result<()> {
    st.validate_for_format(format).map_err(|error| {
        UnityAssetError::with_source(
            format!(
                "SerializedType {} is invalid for SerializedFile v{}",
                st.class_id,
                format.version()
            ),
            error,
        )
    })?;
    if !enable_type_tree
        && (!st.type_tree.is_empty()
            || !st.type_dependencies.is_empty()
            || !st.class_name.is_empty()
            || !st.namespace.is_empty()
            || !st.assembly_name.is_empty())
    {
        return Err(UnityAssetError::format(format!(
            "SerializedType {} contains TypeTree metadata while enableTypeTree is false",
            st.class_id
        )));
    }
    if is_ref_type && !st.type_dependencies.is_empty() {
        return Err(UnityAssetError::format(format!(
            "Reference SerializedType {} cannot encode ordinary type dependencies",
            st.class_id
        )));
    }
    if !is_ref_type
        && (!st.class_name.is_empty() || !st.namespace.is_empty() || !st.assembly_name.is_empty())
    {
        return Err(UnityAssetError::format(format!(
            "Ordinary SerializedType {} cannot encode reference type names",
            st.class_id
        )));
    }
    if is_ref_type
        && !format.has_ref_type_names()
        && (!st.class_name.is_empty() || !st.namespace.is_empty() || !st.assembly_name.is_empty())
    {
        return Err(UnityAssetError::format(format!(
            "SerializedFile v{} cannot encode reference type names",
            format.version()
        )));
    }
    if !format.serialized_type_has_script_id(st.class_id, st.script_type_index, is_ref_type)
        && st.script_id != [0; 16]
    {
        return Err(UnityAssetError::format(format!(
            "SerializedType {} has a script ID that SerializedFile v{} cannot encode",
            st.class_id,
            format.version()
        )));
    }
    if enable_type_tree {
        TypeTreeParser::validate_for_format(&st.type_tree, format).map_err(|error| {
            UnityAssetError::with_source(
                format!("SerializedType {} has an invalid TypeTree", st.class_id),
                error,
            )
        })?;
    }
    writer.write_i32(st.class_id)?;

    if format.serialized_types_have_stripped_flag() {
        writer.write_bool(st.is_stripped_type)?;
    }

    if format.serialized_types_have_script_type_index() {
        writer.write_i16(st.script_type_index)?;
    }

    if format.serialized_types_have_hashes() {
        if format.serialized_type_has_script_id(st.class_id, st.script_type_index, is_ref_type) {
            writer.write(st.script_id.as_slice())?;
        }
        writer.write(st.old_type_hash.as_slice())?;
    }

    if enable_type_tree {
        dump_typetree_to(&st.type_tree, writer, format)?;

        if is_ref_type && format.has_ref_type_names() {
            writer.write_string_to_null(&st.class_name)?;
            writer.write_string_to_null(&st.namespace)?;
            writer.write_string_to_null(&st.assembly_name)?;
        } else if !is_ref_type && format.has_type_dependencies() {
            let count = i32::try_from(st.type_dependencies.len()).map_err(|_| {
                UnityAssetError::format(format!(
                    "type_dependencies too large: {}",
                    st.type_dependencies.len()
                ))
            })?;
            writer.write_i32(count)?;
            for dependency in &st.type_dependencies {
                writer.write_i32(*dependency)?;
            }
        }
    }

    Ok(())
}
