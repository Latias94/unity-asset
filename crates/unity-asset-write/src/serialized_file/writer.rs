use crate::Result;
use crate::binary_writer::{BinaryWriter, Endian};
use crate::serialized_file::edit::SerializedFileEdits;
use crate::serialized_file::external_table::PlannedExternalTable;
use crate::serialized_file::types_write::{
    write_file_identifier, write_local_serialized_object_identifier, write_serialized_type,
};
use unity_asset_binary::asset::{
    HeaderLayout, MetadataField, MetadataPlacement, ObjectInfo, ObjectMetadata,
    ObjectOffsetEncoding, ObjectTailEncoding, ObjectTypeReference, PathIdEncoding, SerializedFile,
    SerializedFileFormat,
};
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
        let format = file.format();
        if file.header.version != format.version() {
            return Err(UnityAssetError::format(format!(
                "SerializedFile header version {} disagrees with format {}",
                file.header.version,
                format.version()
            )));
        }
        file.validate().map_err(|error| {
            UnityAssetError::with_source("Invalid SerializedFile wire state", error)
        })?;
        validate_representable_file_state(file, format)?;
        let external_table = PlannedExternalTable::build(file, edits).map_err(|error| {
            UnityAssetError::with_source("Invalid SerializedFile external table", error)
        })?;
        for path_id in edits.object_path_ids() {
            if file.find_object(path_id).is_none() {
                return Err(UnityAssetError::format(format!(
                    "SerializedFile edit references unknown object path ID {path_id}"
                )));
            }
        }

        let endian = match file.header.byte_order() {
            unity_asset_binary::reader::ByteOrder::Little => Endian::Little,
            unity_asset_binary::reader::ByteOrder::Big => Endian::Big,
        };

        let mut meta = BinaryWriter::new(endian);
        let mut data = BinaryWriter::new(endian);

        if format.has_metadata_field(MetadataField::UnityVersion) {
            meta.write_string_to_null(&file.unity_version);
        }

        if format.has_metadata_field(MetadataField::TargetPlatform) {
            meta.write_i32(file.target_platform);
        }

        if format.has_metadata_field(MetadataField::EnableTypeTree) {
            meta.write_bool(file.type_tree_enabled());
        } else if !file.type_tree_enabled() {
            return Err(UnityAssetError::format(format!(
                "SerializedFile v{} has implicit TypeTree enablement",
                format.version()
            )));
        }

        write_count(&mut meta, "SerializedType", file.types().len())?;
        for st in file.types() {
            write_serialized_type(st, &mut meta, format, file.type_tree_enabled(), false)?;
        }

        if format.has_metadata_field(MetadataField::BigIdEnabled) {
            let raw = file.legacy_big_id().ok_or_else(|| {
                UnityAssetError::format(format!(
                    "SerializedFile v{} is missing its raw bigIdEnabled value",
                    format.version()
                ))
            })?;
            meta.write_i32(raw);
        } else if file.legacy_big_id().is_some() {
            return Err(UnityAssetError::format(format!(
                "SerializedFile v{} cannot encode bigIdEnabled",
                format.version()
            )));
        }

        write_count(&mut meta, "object", file.objects().len())?;

        for info in file.objects() {
            write_object_entry(file, format, info, edits, &mut meta, &mut data, options)?;
            data.align_stream(8);
        }

        if format.has_metadata_field(MetadataField::ScriptTypes) {
            write_count(&mut meta, "script type", file.script_types.len())?;
            for s in &file.script_types {
                write_local_serialized_object_identifier(s, &mut meta, format)?;
            }
        } else if !file.script_types.is_empty() {
            return Err(UnityAssetError::format(format!(
                "SerializedFile v{} cannot encode script types",
                format.version()
            )));
        }

        write_count(&mut meta, "external", external_table.len())?;
        for e in external_table.iter() {
            write_file_identifier(e, &mut meta, format)?;
        }

        if format.has_metadata_field(MetadataField::RefTypes) {
            write_count(&mut meta, "reference type", file.ref_types().len())?;
            for st in file.ref_types() {
                write_serialized_type(st, &mut meta, format, file.type_tree_enabled(), true)?;
            }
        } else if !file.ref_types().is_empty() {
            return Err(UnityAssetError::format(format!(
                "SerializedFile v{} cannot encode reference types",
                format.version()
            )));
        }

        if format.has_metadata_field(MetadataField::UserInformation) {
            meta.write_string_to_null(&file.user_information);
        } else if !file.user_information.is_empty() {
            return Err(UnityAssetError::format(format!(
                "SerializedFile v{} cannot encode user information",
                format.version()
            )));
        }

        meta.ensure_valid()?;
        data.ensure_valid()?;

        let metadata_body_size = u64::try_from(meta.len())
            .map_err(|_| UnityAssetError::format("metadata size does not fit u64"))?;
        let data_size = u64::try_from(data.len())
            .map_err(|_| UnityAssetError::format("data size does not fit u64"))?;
        let legacy_hint = matches!(
            format.metadata_placement(),
            MetadataPlacement::TailWithEndianPrefix
        )
        .then_some(file.header.data_offset);
        let layout = format
            .plan_layout(metadata_body_size, data_size, legacy_hint)
            .map_err(|error| {
                UnityAssetError::with_source("Failed to plan SerializedFile layout", error)
            })?;

        let mut out = BinaryWriter::new(Endian::Big);
        match format.header_layout() {
            HeaderLayout::Legacy16 => {
                out.write_u32(layout.metadata_size);
                out.write_u32(u32_field(layout.file_size, "file size")?);
                out.write_u32(format.version());
                out.write_u32(u32_field(layout.data_offset, "data offset")?);
                pad_to(&mut out, layout.data_offset, "legacy data offset")?;
                out.write(data.bytes());
                out.write_u8(file.header.endian);
                out.write(meta.bytes());
            }
            HeaderLayout::Standard20 => {
                out.write_u32(layout.metadata_size);
                out.write_u32(u32_field(layout.file_size, "file size")?);
                out.write_u32(format.version());
                out.write_u32(u32_field(layout.data_offset, "data offset")?);
                out.write_u8(file.header.endian);
                out.write(&file.header.reserved);
                out.write(meta.bytes());
                pad_to(&mut out, layout.data_offset, "data offset")?;
                out.write(data.bytes());
            }
            HeaderLayout::LargeFiles48 => {
                out.write_u32(0);
                out.write_u32(0);
                out.write_u32(format.version());
                out.write_u32(0);
                out.write_u8(file.header.endian);
                out.write(&file.header.reserved);
                out.write_u32(layout.metadata_size);
                out.write_i64(i64_field(layout.file_size, "file size")?);
                out.write_i64(i64_field(layout.data_offset, "data offset")?);
                out.write_i64(file.header.unknown);
                out.write(meta.bytes());
                pad_to(&mut out, layout.data_offset, "data offset")?;
                out.write(data.bytes());
            }
        }

        out.ensure_valid()?;
        let actual_size = u64::try_from(out.len())
            .map_err(|_| UnityAssetError::format("encoded file size does not fit u64"))?;
        if actual_size != layout.file_size {
            return Err(UnityAssetError::format(format!(
                "SerializedFile layout planned {} bytes but encoded {actual_size}",
                layout.file_size
            )));
        }

        out.into_result()
    }
}

fn write_object_entry(
    file: &SerializedFile,
    format: SerializedFileFormat,
    info: &ObjectInfo,
    edits: &SerializedFileEdits,
    meta: &mut BinaryWriter,
    data: &mut BinaryWriter,
    options: SerializedFileSaveOptions,
) -> Result<()> {
    info.validate().map_err(|error| {
        UnityAssetError::with_source(format!("Invalid object path ID {}", info.path_id()), error)
    })?;
    match format.path_id_encoding() {
        PathIdEncoding::I32 => meta.write_i32(i32_field(info.path_id(), "path ID")?),
        PathIdEncoding::BigIdFlag if file.uses_big_ids() => meta.write_i64(info.path_id()),
        PathIdEncoding::BigIdFlag => meta.write_i32(i32_field(info.path_id(), "path ID")?),
        PathIdEncoding::AlignedI64 => {
            meta.align_stream(4);
            meta.write_i64(info.path_id());
        }
    }

    let obj_bytes = if let Some(override_bytes) = edits.object_bytes(info.path_id()) {
        override_bytes
    } else if let Some(loaded_data) = info.loaded_data() {
        loaded_data
    } else if options.allow_lazy_object_reads {
        file.object_bytes(info)
            .map_err(|e| UnityAssetError::with_source("Failed to read object bytes", e))?
    } else {
        return Err(UnityAssetError::format(format!(
            "Object {} bytes not loaded (path_id={})",
            info.class_id(),
            info.path_id()
        )));
    };

    let relative_offset = u64::try_from(data.position())
        .map_err(|_| UnityAssetError::format("data stream position does not fit u64"))?;
    match format.object_offset_encoding() {
        ObjectOffsetEncoding::U32 => {
            meta.write_u32(u32_field(relative_offset, "object data offset")?)
        }
        ObjectOffsetEncoding::I64 => {
            meta.write_i64(i64_field(relative_offset, "object data offset")?)
        }
    }

    meta.write_u32(u32::try_from(obj_bytes.len()).map_err(|_| {
        UnityAssetError::format(format!(
            "object {} byte size {} does not fit u32",
            info.path_id(),
            obj_bytes.len()
        ))
    })?);

    let type_reference = info.type_reference();
    let raw_type_reference = type_reference.raw_value().map_err(|error| {
        UnityAssetError::with_source(
            format!("Invalid type reference for object {}", info.path_id()),
            error,
        )
    })?;
    meta.write_i32(raw_type_reference);
    if let ObjectTypeReference::Legacy { class_id_bits, .. } = type_reference {
        meta.write_u16(class_id_bits);
    }
    write_object_metadata(format, info.metadata(), info.path_id(), meta)?;

    data.write(obj_bytes);
    Ok(())
}

fn write_count(writer: &mut BinaryWriter, label: &str, count: usize) -> Result<()> {
    let count = i32::try_from(count)
        .map_err(|_| UnityAssetError::format(format!("{label} count too large: {count}")))?;
    writer.write_i32(count);
    Ok(())
}

fn validate_representable_file_state(
    file: &SerializedFile,
    format: SerializedFileFormat,
) -> Result<()> {
    if file.header.endian > 1 {
        return Err(UnityAssetError::format(format!(
            "Invalid SerializedFile endian flag {}",
            file.header.endian
        )));
    }
    if !format.has_metadata_field(MetadataField::UnityVersion) && !file.unity_version.is_empty() {
        return Err(UnityAssetError::format(format!(
            "SerializedFile v{} cannot encode a Unity version string",
            format.version()
        )));
    }
    if !format.has_metadata_field(MetadataField::TargetPlatform) && file.target_platform != 0 {
        return Err(UnityAssetError::format(format!(
            "SerializedFile v{} cannot encode a target platform",
            format.version()
        )));
    }
    match format.header_layout() {
        HeaderLayout::Legacy16 => {
            if file.header.reserved != [0; 3] || file.header.unknown != 0 {
                return Err(UnityAssetError::format(format!(
                    "SerializedFile v{} cannot encode reserved or extended header fields",
                    format.version()
                )));
            }
        }
        HeaderLayout::Standard20 if file.header.unknown != 0 => {
            return Err(UnityAssetError::format(format!(
                "SerializedFile v{} cannot encode the extended unknown header field",
                format.version()
            )));
        }
        HeaderLayout::Standard20 | HeaderLayout::LargeFiles48 => {}
    }
    Ok(())
}

fn write_object_metadata(
    format: SerializedFileFormat,
    metadata: ObjectMetadata,
    path_id: i64,
    writer: &mut BinaryWriter,
) -> Result<()> {
    match (format.object_tail_encoding(), metadata) {
        (ObjectTailEncoding::Destroyed, ObjectMetadata::Destroyed { value }) => {
            writer.write_u16(value)
        }
        (ObjectTailEncoding::ScriptTypeIndex, ObjectMetadata::ScriptTypeIndex { index }) => {
            writer.write_i16(index)
        }
        (
            ObjectTailEncoding::ScriptTypeIndexAndStripped,
            ObjectMetadata::ScriptTypeIndexAndStripped { index, stripped },
        ) => {
            writer.write_i16(index);
            writer.write_u8(stripped);
        }
        (ObjectTailEncoding::None, ObjectMetadata::None) => {}
        (expected, actual) => {
            return Err(UnityAssetError::format(format!(
                "Object {path_id} metadata {actual:?} is incompatible with {expected:?}"
            )));
        }
    }
    Ok(())
}

fn pad_to(writer: &mut BinaryWriter, offset: u64, label: &str) -> Result<()> {
    let target = usize::try_from(offset)
        .map_err(|_| UnityAssetError::format(format!("{label} does not fit usize: {offset}")))?;
    if writer.position() > target {
        return Err(UnityAssetError::format(format!(
            "{label} {target} precedes encoded position {}",
            writer.position()
        )));
    }
    writer.set_position(target);
    Ok(())
}

fn i32_field(value: i64, label: &str) -> Result<i32> {
    i32::try_from(value)
        .map_err(|_| UnityAssetError::format(format!("{label} does not fit i32: {value}")))
}

fn u32_field(value: u64, label: &str) -> Result<u32> {
    u32::try_from(value)
        .map_err(|_| UnityAssetError::format(format!("{label} does not fit u32: {value}")))
}

fn i64_field(value: u64, label: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| UnityAssetError::format(format!("{label} does not fit i64: {value}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const V22_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v22.assets.bin");

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
        assert_eq!(reparsed.type_tree_enabled(), sf.type_tree_enabled());
        assert_eq!(reparsed.types().len(), sf.types().len());
        assert_eq!(reparsed.objects().len(), sf.objects().len());
        assert_eq!(reparsed.externals.len(), sf.externals.len());
        assert_eq!(reparsed.ref_types().len(), sf.ref_types().len());
    }

    #[test]
    fn loaded_empty_payload_is_distinct_from_unloaded_for_strict_saves() {
        let mut file =
            unity_asset_binary::asset::SerializedFileParser::from_bytes(V22_FIXTURE.to_vec())
                .unwrap();
        let path_id = file.objects()[0].path_id();
        file.find_object_mut(path_id).unwrap().set_data(Vec::new());

        let options = SerializedFileSaveOptions {
            allow_lazy_object_reads: false,
        };
        let encoded =
            SerializedFileWriter::save_with_options(&file, &SerializedFileEdits::new(), options)
                .expect("an explicitly loaded empty payload is savable");
        let reparsed =
            unity_asset_binary::asset::SerializedFileParser::from_bytes(encoded).unwrap();
        assert_eq!(reparsed.objects()[0].byte_size(), 0);
        assert!(
            reparsed
                .find_object_handle(path_id)
                .unwrap()
                .raw_data()
                .unwrap()
                .is_empty()
        );

        file.find_object_mut(path_id).unwrap().clear_data();
        let error =
            SerializedFileWriter::save_with_options(&file, &SerializedFileEdits::new(), options)
                .expect_err("an unloaded payload cannot be saved when lazy reads are disabled");
        assert!(error.to_string().contains("bytes not loaded"));
    }
}
