use std::io;
use std::ops::Range;

use unity_asset_binary::BinaryError;
use unity_asset_binary::asset::{
    HeaderLayout, MetadataField, MetadataPlacement, ObjectInfo, ObjectMetadata,
    ObjectOffsetEncoding, ObjectTailEncoding, ObjectTypeReference, PathIdEncoding, SerializedFile,
    SerializedFileFormat, SerializedFileLayout,
};
use unity_asset_binary::shared_bytes::SharedBytes;
use unity_asset_core::UnityAssetError;

use crate::Endian;
use crate::Result;
use crate::artifact::{
    ArtifactBatch, ArtifactBuildError, ArtifactHandle, ArtifactPayload, ArtifactPayloadProvenance,
};
use crate::serialized_file::edit::SerializedFileEdits;
use crate::serialized_file::external_table::PlannedExternalTable;
use crate::serialized_file::sink::{CountingSink, EndianSink, IoSink, SinkBackend};
use crate::serialized_file::types_write::{
    write_file_identifier_to, write_local_serialized_object_identifier_to, write_serialized_type_to,
};
use crate::serialized_file::writer::SerializedFileWriter;

macro_rules! append_object_data {
    ($encoder:expr, $plan:expr) => {{
        let mut data_cursor = 0_u64;
        for info in $plan.file.objects() {
            let data = select_object_data(info, $plan.edits, $plan.source.as_ref())
                .map_err(artifact_error)?;
            let data_len = data.len().map_err(artifact_error)?;
            let aligned_end = align_up(
                data_cursor.checked_add(data_len).ok_or(
                    ArtifactBuildError::ArithmeticOverflow {
                        resource: "serialized_file_data_size",
                    },
                )?,
                8,
            )
            .map_err(artifact_error)?;
            let padding_after = aligned_end - data_cursor - data_len;
            match data {
                ObjectData::Source { payload, range } => {
                    if !range.is_empty() {
                        $encoder.push_payload_range(payload, range)?;
                    }
                    if padding_after != 0 {
                        let mut generated = $encoder.generated_chunk_writer()?;
                        {
                            let mut sink =
                                EndianSink::new(IoSink::new(&mut generated), $plan.endian);
                            write_zeroes(&mut sink, padding_after).map_err(artifact_error)?;
                        }
                        let payload = $encoder.finish_generated_chunk(generated)?;
                        $encoder.push_payload_full(&payload)?;
                    }
                }
                ObjectData::Generated(bytes) => {
                    if !bytes.is_empty() || padding_after != 0 {
                        let mut generated = $encoder.generated_chunk_writer()?;
                        {
                            let mut sink =
                                EndianSink::new(IoSink::new(&mut generated), $plan.endian);
                            sink.write(bytes).map_err(artifact_error)?;
                            write_zeroes(&mut sink, padding_after).map_err(artifact_error)?;
                        }
                        let payload = $encoder.finish_generated_chunk(generated)?;
                        $encoder.push_payload_full(&payload)?;
                    }
                }
            }
            data_cursor = aligned_end;
        }
        if data_cursor != $plan.data_size {
            return Err(ArtifactBuildError::InternalInvariant {
                message: "SerializedFile data sizing pass disagrees with encoding pass",
            });
        }
    }};
}

/// Verified source image and the complete SerializedFile range within it.
///
/// The range is relative to `payload`, not to an enclosing parser view.  It must exactly match the
/// bytes retained by `file`; object ranges are derived only after that binding is verified.
#[derive(Debug, Clone)]
pub struct SerializedFileSource<'source> {
    payload: &'source ArtifactPayload,
    range: Range<usize>,
}

impl<'source> SerializedFileSource<'source> {
    pub fn new(payload: &'source ArtifactPayload, range: Range<usize>) -> Self {
        Self { payload, range }
    }

    pub fn whole(payload: &'source ArtifactPayload) -> Result<Self> {
        let end = usize::try_from(payload.len()).map_err(|_| {
            UnityAssetError::format(format!(
                "SerializedFile source length does not fit usize: {}",
                payload.len()
            ))
        })?;
        Ok(Self::new(payload, 0..end))
    }
}

impl SerializedFileWriter {
    /// Builds one exact, independently inspected SerializedFile proof image.
    ///
    /// Unmodified unloaded object bytes must come from `source`; explicit edits and preloaded
    /// object payloads are copied once into budgeted generated chunks.  Header, metadata, offsets,
    /// and alignment are sized without staging a complete output `Vec`.
    pub fn prepare(
        batch: &mut ArtifactBatch<'_, '_>,
        file: &SerializedFile,
        edits: &SerializedFileEdits,
        source: Option<SerializedFileSource<'_>>,
    ) -> std::result::Result<ArtifactHandle, ArtifactBuildError> {
        let plan = SerializedFilePlan::build(batch, file, edits, source).map_err(artifact_error)?;
        let declared_len = plan.layout.file_size;

        batch.prepare_serialized_file(declared_len, |encoder| {
            match plan.format.header_layout() {
                HeaderLayout::Legacy16 => {
                    let mut generated = encoder.generated_chunk_writer()?;
                    {
                        let mut sink = EndianSink::new(IoSink::new(&mut generated), Endian::Big);
                        write_header(&mut sink, plan.file, plan.format, plan.layout)
                            .map_err(artifact_error)?;
                        pad_to(&mut sink, plan.layout.data_offset, "legacy data offset")
                            .map_err(artifact_error)?;
                    }
                    let payload = encoder.finish_generated_chunk(generated)?;
                    encoder.push_payload_full(&payload)?;

                    append_object_data!(encoder, &plan);

                    let mut generated = encoder.generated_chunk_writer()?;
                    {
                        let mut sink = EndianSink::new(IoSink::new(&mut generated), plan.endian);
                        sink.write_u8(plan.file.header.endian)
                            .map_err(artifact_error)?;
                        write_metadata(&mut sink, &plan).map_err(artifact_error)?;
                    }
                    let payload = encoder.finish_generated_chunk(generated)?;
                    encoder.push_payload_full(&payload)?;
                }
                HeaderLayout::Standard20 | HeaderLayout::LargeFiles48 => {
                    let mut generated = encoder.generated_chunk_writer()?;
                    {
                        let mut sink = EndianSink::new(IoSink::new(&mut generated), Endian::Big);
                        write_header(&mut sink, plan.file, plan.format, plan.layout)
                            .map_err(artifact_error)?;
                    }
                    let payload = encoder.finish_generated_chunk(generated)?;
                    encoder.push_payload_full(&payload)?;

                    let mut generated = encoder.generated_chunk_writer()?;
                    {
                        let mut sink = EndianSink::new(IoSink::new(&mut generated), plan.endian);
                        write_metadata(&mut sink, &plan).map_err(artifact_error)?;
                        let prefix_len = plan
                            .layout
                            .data_offset
                            .checked_sub(plan.layout.header_size)
                            .ok_or(ArtifactBuildError::InternalInvariant {
                                message: "SerializedFile data offset precedes its header",
                            })?;
                        pad_to(&mut sink, prefix_len, "data offset").map_err(artifact_error)?;
                    }
                    let payload = encoder.finish_generated_chunk(generated)?;
                    encoder.push_payload_full(&payload)?;

                    append_object_data!(encoder, &plan);
                }
            }
            Ok(())
        })
    }
}

struct SerializedFilePlan<'source> {
    file: &'source SerializedFile,
    edits: &'source SerializedFileEdits,
    source: Option<SerializedFileSource<'source>>,
    format: SerializedFileFormat,
    endian: Endian,
    layout: SerializedFileLayout,
    data_size: u64,
    external_table: PlannedExternalTable<'source>,
}

enum ObjectData<'source> {
    Source {
        payload: &'source ArtifactPayload,
        range: Range<usize>,
    },
    Generated(&'source [u8]),
}

impl<'source> SerializedFilePlan<'source> {
    fn build(
        batch: &mut ArtifactBatch<'_, '_>,
        file: &'source SerializedFile,
        edits: &'source SerializedFileEdits,
        source: Option<SerializedFileSource<'source>>,
    ) -> Result<Self> {
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
        validate_source_binding(batch, file, source.as_ref())?;

        for path_id in edits.object_path_ids() {
            if file.find_object(path_id).is_none() {
                return Err(UnityAssetError::format(format!(
                    "SerializedFile edit references unknown object path ID {path_id}"
                )));
            }
        }

        let external_table = PlannedExternalTable::build(file, edits).map_err(|error| {
            UnityAssetError::with_source("Invalid SerializedFile external table", error)
        })?;

        let endian = match file.header.byte_order() {
            unity_asset_binary::reader::ByteOrder::Little => Endian::Little,
            unity_asset_binary::reader::ByteOrder::Big => Endian::Big,
        };
        let mut data_cursor = 0_u64;
        for info in file.objects() {
            info.validate().map_err(|error| {
                UnityAssetError::with_source(
                    format!("Invalid object path ID {}", info.path_id()),
                    error,
                )
            })?;
            let data = select_object_data(info, edits, source.as_ref())?;
            let data_len = data.len()?;
            u32::try_from(data_len).map_err(|_| {
                UnityAssetError::format(format!(
                    "object {} byte size {data_len} does not fit u32",
                    info.path_id()
                ))
            })?;
            data_cursor = align_up(
                data_cursor.checked_add(data_len).ok_or_else(|| {
                    UnityAssetError::format("SerializedFile data stream length overflow")
                })?,
                8,
            )?;
        }

        let mut plan = Self {
            file,
            edits,
            source,
            format,
            endian,
            layout: SerializedFileLayout {
                header_size: 0,
                metadata_size: 0,
                data_offset: 0,
                file_size: 0,
            },
            data_size: data_cursor,
            external_table,
        };
        let mut metadata = EndianSink::new(CountingSink::default(), endian);
        write_metadata(&mut metadata, &plan)?;
        let metadata_body_size = metadata.into_inner().length();
        let legacy_hint = matches!(
            format.metadata_placement(),
            MetadataPlacement::TailWithEndianPrefix
        )
        .then_some(file.header.data_offset);
        plan.layout = format
            .plan_layout(metadata_body_size, data_cursor, legacy_hint)
            .map_err(|error| {
                UnityAssetError::with_source("Failed to plan SerializedFile layout", error)
            })?;
        Ok(plan)
    }
}

impl ObjectData<'_> {
    fn len(&self) -> Result<u64> {
        let length = match self {
            Self::Source { range, .. } => range.len(),
            Self::Generated(bytes) => bytes.len(),
        };
        u64::try_from(length)
            .map_err(|_| UnityAssetError::format("object byte size does not fit u64"))
    }
}

fn select_object_data<'source>(
    info: &'source ObjectInfo,
    edits: &'source SerializedFileEdits,
    source: Option<&SerializedFileSource<'source>>,
) -> Result<ObjectData<'source>> {
    if let Some(bytes) = edits.object_bytes(info.path_id()) {
        return Ok(ObjectData::Generated(bytes));
    }
    if let Some(bytes) = info.loaded_data() {
        return Ok(ObjectData::Generated(bytes));
    }
    let source = source.ok_or_else(|| {
        UnityAssetError::format(format!(
            "Object {} bytes are unloaded and no verified SerializedFile source was supplied",
            info.path_id()
        ))
    })?;
    let object_start = usize::try_from(info.byte_start()).map_err(|_| {
        UnityAssetError::format(format!(
            "object {} data offset does not fit usize",
            info.path_id()
        ))
    })?;
    let start = source
        .range
        .start
        .checked_add(object_start)
        .ok_or_else(|| UnityAssetError::format("SerializedFile source object range overflow"))?;
    let end = start
        .checked_add(info.byte_size() as usize)
        .ok_or_else(|| UnityAssetError::format("SerializedFile source object end overflow"))?;
    if end > source.range.end {
        return Err(UnityAssetError::format(format!(
            "object {} source range {start}..{end} exceeds SerializedFile source {}..{}",
            info.path_id(),
            source.range.start,
            source.range.end
        )));
    }
    Ok(ObjectData::Source {
        payload: source.payload,
        range: start..end,
    })
}

fn validate_source_binding(
    batch: &mut ArtifactBatch<'_, '_>,
    file: &SerializedFile,
    source: Option<&SerializedFileSource<'_>>,
) -> Result<()> {
    let Some(source) = source else {
        return Ok(());
    };
    match source.payload.provenance() {
        ArtifactPayloadProvenance::Source { .. } => {}
        ArtifactPayloadProvenance::Generated => {
            return Err(UnityAssetError::format(
                "SerializedFile source payload must be verified source bytes",
            ));
        }
    }
    let bytes = source
        .payload
        .bytes()
        .get(source.range.clone())
        .ok_or_else(|| {
            UnityAssetError::format(format!(
                "SerializedFile source range {}..{} exceeds payload length {}",
                source.range.start,
                source.range.end,
                source.payload.len()
            ))
        })?;
    let file_end = file
        .data_base_offset()
        .checked_add(file.data().len())
        .ok_or_else(|| UnityAssetError::format("SerializedFile backing range overflow"))?;
    if let SharedBytes::Arc(backing) = file.data_shared()
        && source.payload.shares_shared_backing(&backing)
        && source.range == (file.data_base_offset()..file_end)
    {
        return Ok(());
    }
    let compared_bytes = u64::try_from(bytes.len()).map_err(|_| {
        UnityAssetError::format("SerializedFile comparison length does not fit u64")
    })?;
    batch
        .consume_inspection_bytes(compared_bytes)
        .map_err(|error| {
            UnityAssetError::with_source(
                "SerializedFile detached source comparison exceeded its inspection budget",
                error,
            )
        })?;
    if bytes != file.data() {
        return Err(UnityAssetError::format(
            "SerializedFile source range does not match the parsed file image",
        ));
    }
    Ok(())
}

fn write_metadata<B: SinkBackend>(
    writer: &mut EndianSink<B>,
    plan: &SerializedFilePlan<'_>,
) -> Result<()> {
    let file = plan.file;
    let format = plan.format;
    if format.has_metadata_field(MetadataField::UnityVersion) {
        writer.write_string_to_null(&file.unity_version)?;
    }
    if format.has_metadata_field(MetadataField::TargetPlatform) {
        writer.write_i32(file.target_platform)?;
    }
    if format.has_metadata_field(MetadataField::EnableTypeTree) {
        writer.write_bool(file.type_tree_enabled())?;
    }

    write_count(writer, "SerializedType", file.types().len())?;
    for serialized_type in file.types() {
        write_serialized_type_to(
            serialized_type,
            writer,
            format,
            file.type_tree_enabled(),
            false,
        )?;
    }

    if format.has_metadata_field(MetadataField::BigIdEnabled) {
        let raw = file.legacy_big_id().ok_or_else(|| {
            UnityAssetError::format(format!(
                "SerializedFile v{} is missing its raw bigIdEnabled value",
                format.version()
            ))
        })?;
        writer.write_i32(raw)?;
    }

    write_count(writer, "object", file.objects().len())?;
    let mut data_cursor = 0_u64;
    for info in file.objects() {
        let data = select_object_data(info, plan.edits, plan.source.as_ref())?;
        let data_len = data.len()?;
        let byte_size = u32::try_from(data_len).map_err(|_| {
            UnityAssetError::format(format!(
                "object {} byte size {data_len} does not fit u32",
                info.path_id()
            ))
        })?;
        write_object_entry(writer, file, format, info, data_cursor, byte_size)?;
        data_cursor = align_up(
            data_cursor
                .checked_add(data_len)
                .ok_or_else(|| UnityAssetError::format("SerializedFile data size overflow"))?,
            8,
        )?;
    }
    if data_cursor != plan.data_size {
        return Err(UnityAssetError::format(
            "SerializedFile metadata sizing disagrees with object data sizing",
        ));
    }

    if format.has_metadata_field(MetadataField::ScriptTypes) {
        write_count(writer, "script type", file.script_types.len())?;
        for script_type in &file.script_types {
            write_local_serialized_object_identifier_to(script_type, writer, format)?;
        }
    }

    write_count(writer, "external", plan.external_table.len())?;
    for external in plan.external_table.iter() {
        write_file_identifier_to(external, writer, format)?;
    }

    if format.has_metadata_field(MetadataField::RefTypes) {
        write_count(writer, "reference type", file.ref_types().len())?;
        for serialized_type in file.ref_types() {
            write_serialized_type_to(
                serialized_type,
                writer,
                format,
                file.type_tree_enabled(),
                true,
            )?;
        }
    }

    if format.has_metadata_field(MetadataField::UserInformation) {
        writer.write_string_to_null(&file.user_information)?;
    }
    Ok(())
}

fn write_object_entry<B: SinkBackend>(
    writer: &mut EndianSink<B>,
    file: &SerializedFile,
    format: SerializedFileFormat,
    info: &ObjectInfo,
    relative_offset: u64,
    byte_size: u32,
) -> Result<()> {
    match format.path_id_encoding() {
        PathIdEncoding::I32 => writer.write_i32(i32_field(info.path_id(), "path ID")?)?,
        PathIdEncoding::BigIdFlag if file.uses_big_ids() => writer.write_i64(info.path_id())?,
        PathIdEncoding::BigIdFlag => writer.write_i32(i32_field(info.path_id(), "path ID")?)?,
        PathIdEncoding::AlignedI64 => {
            writer.align_stream(4)?;
            writer.write_i64(info.path_id())?;
        }
    }
    match format.object_offset_encoding() {
        ObjectOffsetEncoding::U32 => {
            writer.write_u32(u32_field(relative_offset, "object data offset")?)?
        }
        ObjectOffsetEncoding::I64 => {
            writer.write_i64(i64_field(relative_offset, "object data offset")?)?
        }
    }
    writer.write_u32(byte_size)?;
    let type_reference = info.type_reference();
    let raw_type_reference = type_reference.raw_value().map_err(|error| {
        UnityAssetError::with_source(
            format!("Invalid type reference for object {}", info.path_id()),
            error,
        )
    })?;
    writer.write_i32(raw_type_reference)?;
    if let ObjectTypeReference::Legacy { class_id_bits, .. } = type_reference {
        writer.write_u16(class_id_bits)?;
    }
    write_object_metadata(format, info.metadata(), info.path_id(), writer)
}

fn write_header<B: SinkBackend>(
    writer: &mut EndianSink<B>,
    file: &SerializedFile,
    format: SerializedFileFormat,
    layout: SerializedFileLayout,
) -> Result<()> {
    match format.header_layout() {
        HeaderLayout::Legacy16 => {
            writer.write_u32(layout.metadata_size)?;
            writer.write_u32(u32_field(layout.file_size, "file size")?)?;
            writer.write_u32(format.version())?;
            writer.write_u32(u32_field(layout.data_offset, "data offset")?)?;
        }
        HeaderLayout::Standard20 => {
            writer.write_u32(layout.metadata_size)?;
            writer.write_u32(u32_field(layout.file_size, "file size")?)?;
            writer.write_u32(format.version())?;
            writer.write_u32(u32_field(layout.data_offset, "data offset")?)?;
            writer.write_u8(file.header.endian)?;
            writer.write(&file.header.reserved)?;
        }
        HeaderLayout::LargeFiles48 => {
            writer.write_u32(0)?;
            writer.write_u32(0)?;
            writer.write_u32(format.version())?;
            writer.write_u32(0)?;
            writer.write_u8(file.header.endian)?;
            writer.write(&file.header.reserved)?;
            writer.write_u32(layout.metadata_size)?;
            writer.write_i64(i64_field(layout.file_size, "file size")?)?;
            writer.write_i64(i64_field(layout.data_offset, "data offset")?)?;
            writer.write_i64(file.header.unknown)?;
        }
    }
    let position = writer.position()?;
    if position != layout.header_size {
        return Err(UnityAssetError::format(format!(
            "SerializedFile header planned {} bytes but encoded {position}",
            layout.header_size
        )));
    }
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
    if !format.has_metadata_field(MetadataField::EnableTypeTree) && !file.type_tree_enabled() {
        return Err(UnityAssetError::format(format!(
            "SerializedFile v{} has implicit TypeTree enablement",
            format.version()
        )));
    }
    if !format.has_metadata_field(MetadataField::BigIdEnabled) && file.legacy_big_id().is_some() {
        return Err(UnityAssetError::format(format!(
            "SerializedFile v{} cannot encode bigIdEnabled",
            format.version()
        )));
    }
    if !format.has_metadata_field(MetadataField::ScriptTypes) && !file.script_types.is_empty() {
        return Err(UnityAssetError::format(format!(
            "SerializedFile v{} cannot encode script types",
            format.version()
        )));
    }
    if !format.has_metadata_field(MetadataField::RefTypes) && !file.ref_types().is_empty() {
        return Err(UnityAssetError::format(format!(
            "SerializedFile v{} cannot encode reference types",
            format.version()
        )));
    }
    if !format.has_metadata_field(MetadataField::UserInformation)
        && !file.user_information.is_empty()
    {
        return Err(UnityAssetError::format(format!(
            "SerializedFile v{} cannot encode user information",
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

fn write_object_metadata<B: SinkBackend>(
    format: SerializedFileFormat,
    metadata: ObjectMetadata,
    path_id: i64,
    writer: &mut EndianSink<B>,
) -> Result<()> {
    match (format.object_tail_encoding(), metadata) {
        (ObjectTailEncoding::Destroyed, ObjectMetadata::Destroyed { value }) => {
            writer.write_u16(value)?;
        }
        (ObjectTailEncoding::ScriptTypeIndex, ObjectMetadata::ScriptTypeIndex { index }) => {
            writer.write_i16(index)?;
        }
        (
            ObjectTailEncoding::ScriptTypeIndexAndStripped,
            ObjectMetadata::ScriptTypeIndexAndStripped { index, stripped },
        ) => {
            writer.write_i16(index)?;
            writer.write_u8(stripped)?;
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

fn write_count<B: SinkBackend>(
    writer: &mut EndianSink<B>,
    label: &str,
    count: usize,
) -> Result<()> {
    let count = i32::try_from(count)
        .map_err(|_| UnityAssetError::format(format!("{label} count too large: {count}")))?;
    writer.write_i32(count)
}

fn pad_to<B: SinkBackend>(writer: &mut EndianSink<B>, offset: u64, label: &str) -> Result<()> {
    let position = writer.position()?;
    let padding = offset.checked_sub(position).ok_or_else(|| {
        UnityAssetError::format(format!(
            "{label} {offset} precedes encoded position {position}"
        ))
    })?;
    write_zeroes(writer, padding)
}

fn write_zeroes<B: SinkBackend>(writer: &mut EndianSink<B>, mut length: u64) -> Result<()> {
    const ZEROS: [u8; 4096] = [0; 4096];
    while length != 0 {
        let count = usize::try_from(length.min(ZEROS.len() as u64))
            .map_err(|_| UnityAssetError::format("zero padding length does not fit usize"))?;
        writer.write(&ZEROS[..count])?;
        length -= count as u64;
    }
    Ok(())
}

fn align_up(value: u64, alignment: u64) -> Result<u64> {
    let mask = alignment - 1;
    value
        .checked_add(mask)
        .map(|value| value & !mask)
        .ok_or_else(|| UnityAssetError::format("SerializedFile data alignment overflow"))
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

fn artifact_error(error: UnityAssetError) -> ArtifactBuildError {
    let message = error.to_string();
    match error {
        UnityAssetError::Io(error) => ArtifactBuildError::from(error),
        UnityAssetError::WithSource { source, .. } => {
            let source = match source.downcast::<ArtifactBuildError>() {
                Ok(error) => return *error,
                Err(source) => source,
            };
            if let Ok(error) = source.downcast::<io::Error>() {
                return ArtifactBuildError::from(*error);
            }
            ArtifactBuildError::Binary(BinaryError::invalid_data(message))
        }
        _ => ArtifactBuildError::Binary(BinaryError::invalid_data(message)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use unity_asset_binary::asset::SerializedFileParser;
    use unity_asset_binary::shared_bytes::SharedBytes;
    use unity_asset_core::{
        AssetLoadBudget, AssetLoadLimits, SourceId, SourceKind, VerifiedSourceImage, WorkspaceId,
    };

    use super::*;
    use crate::artifact::{
        ArtifactBatchDeclaration, ArtifactBudget, ArtifactBudgetError, ArtifactLimits,
        LogicalArtifactName, PreparedArtifactFormat,
    };

    const V22_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v22.assets.bin");
    const V2_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v2.assets.bin");
    const V8_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v8.assets.bin");
    const V15_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v15.assets.bin");

    fn source_payload(bytes: &[u8]) -> ArtifactPayload {
        source_payload_with_kind(bytes, SourceKind::SerializedFile)
    }

    fn source_payload_with_kind(bytes: &[u8], kind: SourceKind) -> ArtifactPayload {
        let source = SourceId::new(WorkspaceId::from_u128(41).unwrap(), kind, 1).unwrap();
        let image = VerifiedSourceImage::verify(kind, Arc::<[u8]>::from(bytes));
        ArtifactPayload::source_backed(source, image).unwrap()
    }

    fn source_payload_from_backing(bytes: Arc<[u8]>) -> ArtifactPayload {
        let source = SourceId::new(
            WorkspaceId::from_u128(41).unwrap(),
            SourceKind::SerializedFile,
            1,
        )
        .unwrap();
        let image = VerifiedSourceImage::verify(SourceKind::SerializedFile, bytes);
        ArtifactPayload::source_backed(source, image).unwrap()
    }

    fn batch<'a, 'b>(
        budget: &'a mut ArtifactBudget,
        load: &'b mut AssetLoadBudget,
    ) -> (crate::artifact::OutputSlot, ArtifactBatch<'a, 'b>) {
        let mut declaration = ArtifactBatchDeclaration::begin(budget, load).unwrap();
        let output = declaration
            .declare_output(LogicalArtifactName::new("main.assets").unwrap())
            .unwrap();
        (output, declaration.seal_output_names().unwrap())
    }

    #[test]
    fn prepared_serialized_file_reuses_unmodified_source_object_ranges() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let source = source_payload(V22_FIXTURE);
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load = AssetLoadBudget::default();
        let (output, mut batch) = batch(&mut budget, &mut load);

        let artifact = SerializedFileWriter::prepare(
            &mut batch,
            &file,
            &SerializedFileEdits::default(),
            Some(SerializedFileSource::whole(&source).unwrap()),
        )
        .unwrap();
        batch.bind_output(output, artifact).unwrap();
        let set = batch.finish().unwrap();
        let output = set.outputs().next().unwrap();

        assert!(matches!(
            output.artifact().format(),
            PreparedArtifactFormat::SerializedFile(proof) if proof.version() == 22
        ));
        assert_eq!(set.source_dependencies().len(), 1);
        assert_eq!(
            set.source_dependencies()[0].referenced_bytes(),
            u64::from(file.objects()[0].byte_size())
        );
        let mut encoded = Vec::new();
        output.artifact().stream_verified_to(&mut encoded).unwrap();
        let reparsed = SerializedFileParser::from_bytes(encoded).unwrap();
        assert_eq!(reparsed.objects().len(), file.objects().len());
        assert_eq!(
            reparsed.object_bytes(&reparsed.objects()[0]).unwrap(),
            file.object_bytes(&file.objects()[0]).unwrap()
        );
    }

    #[test]
    fn prepared_serialized_file_reuses_a_verified_enclosing_source_range() {
        const PREFIX: &[u8] = b"bundle-prefix";
        const SUFFIX: &[u8] = b"bundle-suffix";

        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let mut enclosing = Vec::with_capacity(PREFIX.len() + V22_FIXTURE.len() + SUFFIX.len());
        enclosing.extend_from_slice(PREFIX);
        enclosing.extend_from_slice(V22_FIXTURE);
        enclosing.extend_from_slice(SUFFIX);
        let source = source_payload_with_kind(&enclosing, SourceKind::AssetBundle);
        let file_range = PREFIX.len()..PREFIX.len() + V22_FIXTURE.len();
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load = AssetLoadBudget::default();
        let (output, mut batch) = batch(&mut budget, &mut load);

        let artifact = SerializedFileWriter::prepare(
            &mut batch,
            &file,
            &SerializedFileEdits::default(),
            Some(SerializedFileSource::new(&source, file_range)),
        )
        .unwrap();
        batch.bind_output(output, artifact).unwrap();
        let set = batch.finish().unwrap();

        assert_eq!(set.source_dependencies().len(), 1);
        assert_eq!(
            set.source_dependencies()[0].source().kind(),
            SourceKind::AssetBundle
        );
        let mut encoded = Vec::new();
        set.outputs()
            .next()
            .unwrap()
            .artifact()
            .stream_verified_to(&mut encoded)
            .unwrap();
        let reparsed = SerializedFileParser::from_bytes(encoded).unwrap();
        assert_eq!(
            reparsed.object_bytes(&reparsed.objects()[0]).unwrap(),
            file.object_bytes(&file.objects()[0]).unwrap()
        );
    }

    #[test]
    fn source_binding_uses_backing_identity_without_a_comparison_budget() {
        let backing: Arc<[u8]> = Arc::from(V22_FIXTURE);
        let file = SerializedFileParser::from_shared_range(
            SharedBytes::Arc(Arc::clone(&backing)),
            0..backing.len(),
        )
        .unwrap();
        let source = source_payload_from_backing(backing);
        let source = SerializedFileSource::whole(&source).unwrap();
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let (_, mut batch) = batch(&mut budget, &mut load);

        validate_source_binding(&mut batch, &file, Some(&source)).unwrap();
    }

    #[test]
    fn detached_source_binding_charges_the_comparison_before_scanning() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let source = source_payload(V22_FIXTURE);
        let source = SerializedFileSource::whole(&source).unwrap();
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(V22_FIXTURE.len() - 1).unwrap(),
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let (_, mut batch) = batch(&mut budget, &mut load);

        let error = validate_source_binding(&mut batch, &file, Some(&source)).unwrap_err();
        assert!(matches!(
            artifact_error(error),
            ArtifactBuildError::LoadBudget(unity_asset_core::BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
    }

    #[test]
    fn prepared_serialized_file_round_trips_legacy_and_modern_layouts() {
        for bytes in [V2_FIXTURE, V8_FIXTURE, V15_FIXTURE, V22_FIXTURE] {
            let file = SerializedFileParser::from_bytes(bytes.to_vec()).unwrap();
            let source = source_payload(bytes);
            let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
            let mut load = AssetLoadBudget::default();
            let (output, mut batch) = batch(&mut budget, &mut load);

            let artifact = SerializedFileWriter::prepare(
                &mut batch,
                &file,
                &SerializedFileEdits::default(),
                Some(SerializedFileSource::whole(&source).unwrap()),
            )
            .unwrap_or_else(|error| {
                panic!("version {} prepare failed: {error}", file.header.version)
            });
            batch.bind_output(output, artifact).unwrap();
            let set = batch.finish().unwrap();
            let output = set.outputs().next().unwrap();
            let mut encoded = Vec::new();
            output.artifact().stream_verified_to(&mut encoded).unwrap();
            let reparsed = SerializedFileParser::from_bytes(encoded).unwrap();
            assert_eq!(reparsed.header.version, file.header.version);
            assert_eq!(reparsed.objects().len(), file.objects().len());
        }
    }

    #[test]
    fn prepared_serialized_file_uses_generated_bytes_for_edits() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let path_id = file.objects()[0].path_id();
        let replacement = b"replacement".to_vec();
        let mut edits = SerializedFileEdits::default();
        edits
            .try_set_object_bytes(
                path_id,
                replacement.clone(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load = AssetLoadBudget::default();
        let (output, mut batch) = batch(&mut budget, &mut load);

        let artifact = SerializedFileWriter::prepare(&mut batch, &file, &edits, None).unwrap();
        batch.bind_output(output, artifact).unwrap();
        let set = batch.finish().unwrap();
        assert!(set.source_dependencies().is_empty());
        let output = set.outputs().next().unwrap();
        let mut encoded = Vec::new();
        output.artifact().stream_verified_to(&mut encoded).unwrap();
        let reparsed = SerializedFileParser::from_bytes(encoded).unwrap();
        let object = reparsed.find_object(path_id).unwrap();
        assert_eq!(reparsed.object_bytes(object).unwrap(), replacement);
    }

    #[test]
    fn unloaded_object_requires_a_matching_verified_source() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load = AssetLoadBudget::default();
        let (_, mut batch) = batch(&mut budget, &mut load);

        let error =
            SerializedFileWriter::prepare(&mut batch, &file, &SerializedFileEdits::default(), None)
                .expect_err("an unloaded object cannot lose its provenance");

        assert!(
            error
                .to_string()
                .contains("no verified SerializedFile source")
        );
    }

    #[test]
    fn generated_chunk_budget_failure_is_typed_and_poisons_the_batch() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let source = source_payload(V22_FIXTURE);
        let limits = ArtifactLimits::default().with_max_generated_chunk_bytes(8);
        let mut budget = ArtifactBudget::new(limits).unwrap();
        let mut load = AssetLoadBudget::default();
        let (_, mut batch) = batch(&mut budget, &mut load);

        let error = SerializedFileWriter::prepare(
            &mut batch,
            &file,
            &SerializedFileEdits::default(),
            Some(SerializedFileSource::whole(&source).unwrap()),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ArtifactBuildError::Budget(ArtifactBudgetError::Exceeded {
                resource: "generated_chunk_bytes",
                ..
            })
        ));
        assert!(matches!(
            SerializedFileWriter::prepare(
                &mut batch,
                &file,
                &SerializedFileEdits::default(),
                Some(SerializedFileSource::whole(&source).unwrap()),
            ),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
}
