use std::ops::Range;

use unity_asset_binary::asset::{
    HeaderLayout, MetadataField, MetadataPlacement, ObjectInfo, ObjectMetadata,
    ObjectOffsetEncoding, ObjectTailEncoding, ObjectTypeReference, PathIdEncoding, SerializedFile,
    SerializedFileFormat, SerializedFileLayout,
};
use unity_asset_core::UnityAssetError;

use crate::artifact::{ArtifactBatch, ArtifactPayload, ArtifactPayloadProvenance};
use crate::serialized_file::edit::SerializedFileEdits;
use crate::serialized_file::external_table::PlannedExternalTable;
use crate::serialized_file::sink::{CountingSink, EndianSink, SinkBackend};
use crate::serialized_file::types_write::{
    write_file_identifier_to, write_local_serialized_object_identifier_to, write_serialized_type_to,
};
use crate::{BinaryWriter, ByteOrder, Result};

/// Verified source image and the complete SerializedFile range within it.
///
/// The range is relative to `payload`, not to an enclosing parser view. It must exactly match the
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

/// Canonical wire plan shared by contiguous and prepared-artifact adapters.
///
/// The plan owns format validation, exact layout, object-byte selection, and ordered segment
/// emission. Adapters only decide where generated chunks and verified source ranges are stored.
pub(crate) struct SerializedFilePlan<'source> {
    file: &'source SerializedFile,
    edits: &'source SerializedFileEdits,
    object_source: ObjectSource<'source>,
    format: SerializedFileFormat,
    byte_order: ByteOrder,
    layout: SerializedFileLayout,
    metadata_body_size: u64,
    data_size: u64,
    external_table: PlannedExternalTable<'source>,
}

enum ObjectSource<'source> {
    ParsedImage,
    Verified(Option<SerializedFileSource<'source>>),
}

enum ObjectData<'source> {
    BorrowedSourceRange {
        payload: &'source ArtifactPayload,
        range: Range<usize>,
    },
    Generated(&'source [u8]),
}

pub(crate) enum SerializedFileSegment<'source> {
    Generated(GeneratedRegion<'source>),
    BorrowedSourceRange {
        payload: &'source ArtifactPayload,
        range: Range<usize>,
    },
}

pub(crate) enum GeneratedRegion<'source> {
    Header {
        includes_legacy_data_padding: bool,
    },
    Metadata {
        includes_legacy_endian: bool,
        includes_modern_data_padding: bool,
    },
    ObjectAndAlignment {
        bytes: &'source [u8],
        padding_after: u64,
    },
    Alignment {
        length: u64,
    },
}

impl<'source> SerializedFilePlan<'source> {
    pub(crate) fn build_for_save(
        file: &'source SerializedFile,
        edits: &'source SerializedFileEdits,
    ) -> Result<Self> {
        let format = validate_file_state(file)?;
        edits.validate_for(file).map_err(|error| {
            UnityAssetError::with_source("Invalid SerializedFile object edits", error)
        })?;
        Self::build_validated(file, edits, ObjectSource::ParsedImage, format)
    }

    pub(crate) fn build_for_artifact(
        batch: &mut ArtifactBatch<'_, '_>,
        file: &'source SerializedFile,
        edits: &'source SerializedFileEdits,
        source: Option<SerializedFileSource<'source>>,
    ) -> Result<Self> {
        let format = validate_file_state(file)?;
        validate_source_binding(batch, file, source.as_ref())?;
        edits.validate_for(file).map_err(|error| {
            UnityAssetError::with_source("Invalid SerializedFile object edits", error)
        })?;
        Self::build_validated(file, edits, ObjectSource::Verified(source), format)
    }

    fn build_validated(
        file: &'source SerializedFile,
        edits: &'source SerializedFileEdits,
        object_source: ObjectSource<'source>,
        format: SerializedFileFormat,
    ) -> Result<Self> {
        let external_table = PlannedExternalTable::build(file, edits).map_err(|error| {
            UnityAssetError::with_source("Invalid SerializedFile external table", error)
        })?;
        let byte_order = file.header.byte_order();

        let mut data_size = 0_u64;
        for info in file.objects() {
            info.validate().map_err(|error| {
                UnityAssetError::with_source(
                    format!("Invalid object path ID {}", info.path_id()),
                    error,
                )
            })?;
            let data = select_object_data(file, edits, &object_source, info)?;
            let data_len = data.len()?;
            u32::try_from(data_len).map_err(|_| {
                UnityAssetError::format(format!(
                    "object {} byte size {data_len} does not fit u32",
                    info.path_id()
                ))
            })?;
            data_size = align_up(
                data_size.checked_add(data_len).ok_or_else(|| {
                    UnityAssetError::format("SerializedFile data stream length overflow")
                })?,
                8,
            )?;
        }

        let mut plan = Self {
            file,
            edits,
            object_source,
            format,
            byte_order,
            layout: SerializedFileLayout {
                header_size: 0,
                metadata_size: 0,
                data_offset: 0,
                file_size: 0,
            },
            metadata_body_size: 0,
            data_size,
            external_table,
        };
        let mut metadata = EndianSink::new(CountingSink::default(), byte_order);
        plan.write_metadata(&mut metadata)?;
        plan.metadata_body_size = metadata.into_inner().length();
        let legacy_hint = matches!(
            format.metadata_placement(),
            MetadataPlacement::TailWithEndianPrefix
        )
        .then_some(file.header.data_offset);
        plan.layout = format
            .plan_layout(plan.metadata_body_size, data_size, legacy_hint)
            .map_err(|error| {
                UnityAssetError::with_source("Failed to plan SerializedFile layout", error)
            })?;
        Ok(plan)
    }

    pub(crate) const fn declared_len(&self) -> u64 {
        self.layout.file_size
    }

    /// Visits the exact output sequence without retaining a second segment table.
    pub(crate) fn visit_segments(
        &self,
        mut visit: impl FnMut(SerializedFileSegment<'source>) -> Result<()>,
    ) -> Result<()> {
        match self.format.header_layout() {
            HeaderLayout::Legacy16 => {
                visit(SerializedFileSegment::Generated(GeneratedRegion::Header {
                    includes_legacy_data_padding: true,
                }))?;
                self.visit_object_data(&mut visit)?;
                visit(SerializedFileSegment::Generated(
                    GeneratedRegion::Metadata {
                        includes_legacy_endian: true,
                        includes_modern_data_padding: false,
                    },
                ))?;
            }
            HeaderLayout::Standard20 | HeaderLayout::LargeFiles48 => {
                visit(SerializedFileSegment::Generated(GeneratedRegion::Header {
                    includes_legacy_data_padding: false,
                }))?;
                visit(SerializedFileSegment::Generated(
                    GeneratedRegion::Metadata {
                        includes_legacy_endian: false,
                        includes_modern_data_padding: true,
                    },
                ))?;
                self.visit_object_data(&mut visit)?;
            }
        }
        Ok(())
    }

    fn visit_object_data(
        &self,
        visit: &mut impl FnMut(SerializedFileSegment<'source>) -> Result<()>,
    ) -> Result<()> {
        let mut data_cursor = 0_u64;
        for info in self.file.objects() {
            let data = self.object_data(info)?;
            let data_len = data.len()?;
            let aligned_end = align_up(
                data_cursor.checked_add(data_len).ok_or_else(|| {
                    UnityAssetError::format("SerializedFile data stream length overflow")
                })?,
                8,
            )?;
            let padding_after = aligned_end - data_cursor - data_len;
            match data {
                ObjectData::BorrowedSourceRange { payload, range } => {
                    if !range.is_empty() {
                        visit(SerializedFileSegment::BorrowedSourceRange { payload, range })?;
                    }
                    if padding_after != 0 {
                        visit(SerializedFileSegment::Generated(
                            GeneratedRegion::Alignment {
                                length: padding_after,
                            },
                        ))?;
                    }
                }
                ObjectData::Generated(bytes) => {
                    if !bytes.is_empty() || padding_after != 0 {
                        visit(SerializedFileSegment::Generated(
                            GeneratedRegion::ObjectAndAlignment {
                                bytes,
                                padding_after,
                            },
                        ))?;
                    }
                }
            }
            data_cursor = aligned_end;
        }
        if data_cursor != self.data_size {
            return Err(UnityAssetError::format(
                "SerializedFile data sizing pass disagrees with encoding pass",
            ));
        }
        Ok(())
    }

    pub(crate) fn encode_generated<B: SinkBackend>(
        &self,
        region: GeneratedRegion<'_>,
        backend: &mut B,
    ) -> Result<()> {
        let start = backend.position()?;
        let expected_len = self.generated_region_len(&region)?;
        let expected_end = start.checked_add(expected_len).ok_or_else(|| {
            UnityAssetError::format("SerializedFile generated region position overflow")
        })?;
        match region {
            GeneratedRegion::Header {
                includes_legacy_data_padding,
            } => {
                if start != 0 {
                    return Err(UnityAssetError::format(format!(
                        "SerializedFile header must start at zero, got {start}"
                    )));
                }
                let mut writer = EndianSink::new(&mut *backend, ByteOrder::Big);
                write_header(&mut writer, self.file, self.format, self.layout)?;
                if includes_legacy_data_padding {
                    pad_to(&mut writer, expected_end, "legacy data offset")?;
                }
            }
            GeneratedRegion::Metadata {
                includes_legacy_endian,
                includes_modern_data_padding,
            } => {
                let mut writer = EndianSink::new(&mut *backend, self.byte_order);
                if includes_legacy_endian {
                    writer.write_u8(self.file.header.endian)?;
                }
                self.write_metadata(&mut writer)?;
                if includes_modern_data_padding {
                    let prefix_len = self
                        .layout
                        .data_offset
                        .checked_sub(self.layout.header_size)
                        .ok_or_else(|| {
                            UnityAssetError::format(
                                "SerializedFile data offset precedes its header",
                            )
                        })?;
                    let target = start.checked_add(prefix_len).ok_or_else(|| {
                        UnityAssetError::format("SerializedFile data offset overflow")
                    })?;
                    pad_to(&mut writer, target, "data offset")?;
                }
            }
            GeneratedRegion::ObjectAndAlignment {
                bytes,
                padding_after,
            } => {
                let mut writer = EndianSink::new(&mut *backend, self.byte_order);
                writer.write(bytes)?;
                write_zeroes(&mut writer, padding_after)?;
            }
            GeneratedRegion::Alignment { length } => {
                let mut writer = EndianSink::new(&mut *backend, self.byte_order);
                write_zeroes(&mut writer, length)?;
            }
        }
        let actual_end = backend.position()?;
        if actual_end != expected_end {
            return Err(UnityAssetError::format(format!(
                "SerializedFile generated region planned end {expected_end} but encoded end {actual_end}"
            )));
        }
        Ok(())
    }

    fn generated_region_len(&self, region: &GeneratedRegion<'_>) -> Result<u64> {
        match region {
            GeneratedRegion::Header {
                includes_legacy_data_padding: true,
            } => Ok(self.layout.data_offset),
            GeneratedRegion::Header {
                includes_legacy_data_padding: false,
            } => Ok(self.layout.header_size),
            GeneratedRegion::Metadata {
                includes_legacy_endian: true,
                includes_modern_data_padding: false,
            } => self
                .metadata_body_size
                .checked_add(1)
                .ok_or_else(|| UnityAssetError::format("SerializedFile metadata length overflow")),
            GeneratedRegion::Metadata {
                includes_legacy_endian: false,
                includes_modern_data_padding: true,
            } => self
                .layout
                .data_offset
                .checked_sub(self.layout.header_size)
                .ok_or_else(|| {
                    UnityAssetError::format("SerializedFile data offset precedes its header")
                }),
            GeneratedRegion::Metadata { .. } => Err(UnityAssetError::format(
                "Invalid SerializedFile metadata region configuration",
            )),
            GeneratedRegion::ObjectAndAlignment {
                bytes,
                padding_after,
            } => u64::try_from(bytes.len())
                .map_err(|_| UnityAssetError::format("object byte size does not fit u64"))?
                .checked_add(*padding_after)
                .ok_or_else(|| UnityAssetError::format("object data length overflow")),
            GeneratedRegion::Alignment { length } => Ok(*length),
        }
    }

    pub(crate) fn encode_to_vec(&self) -> Result<Vec<u8>> {
        let mut output = BinaryWriter::new(ByteOrder::Big);
        self.visit_segments(|segment| {
            match segment {
                SerializedFileSegment::Generated(region) => {
                    self.encode_generated(region, &mut output)?;
                }
                SerializedFileSegment::BorrowedSourceRange { payload, range } => {
                    let bytes = payload.bytes().get(range.clone()).ok_or_else(|| {
                        UnityAssetError::format(format!(
                            "SerializedFile source range {}..{} exceeds payload length {}",
                            range.start,
                            range.end,
                            payload.len()
                        ))
                    })?;
                    output.write(bytes);
                }
            }
            output.ensure_valid()
        })?;
        let actual_len = u64::try_from(output.len())
            .map_err(|_| UnityAssetError::format("encoded file size does not fit u64"))?;
        if actual_len != self.layout.file_size {
            return Err(UnityAssetError::format(format!(
                "SerializedFile layout planned {} bytes but encoded {actual_len}",
                self.layout.file_size
            )));
        }
        output.into_result()
    }

    fn object_data(&self, info: &'source ObjectInfo) -> Result<ObjectData<'source>> {
        select_object_data(self.file, self.edits, &self.object_source, info)
    }

    fn write_metadata<B: SinkBackend>(&self, writer: &mut EndianSink<B>) -> Result<()> {
        let file = self.file;
        let format = self.format;
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
            let data = self.object_data(info)?;
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
        if data_cursor != self.data_size {
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

        write_count(writer, "external", self.external_table.len())?;
        for external in self.external_table.iter() {
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
}

impl ObjectData<'_> {
    fn len(&self) -> Result<u64> {
        let length = match self {
            Self::BorrowedSourceRange { range, .. } => range.len(),
            Self::Generated(bytes) => bytes.len(),
        };
        u64::try_from(length)
            .map_err(|_| UnityAssetError::format("object byte size does not fit u64"))
    }
}

fn select_object_data<'source>(
    file: &'source SerializedFile,
    edits: &'source SerializedFileEdits,
    object_source: &ObjectSource<'source>,
    info: &'source ObjectInfo,
) -> Result<ObjectData<'source>> {
    if let Some(bytes) = edits.object_bytes(info.path_id()) {
        return Ok(ObjectData::Generated(bytes));
    }
    if let Some(bytes) = info.loaded_data() {
        return Ok(ObjectData::Generated(bytes));
    }
    let source = match object_source {
        ObjectSource::ParsedImage => {
            return file
                .object_bytes(info)
                .map(ObjectData::Generated)
                .map_err(|error| {
                    UnityAssetError::with_source("Failed to read object bytes", error)
                });
        }
        ObjectSource::Verified(Some(source)) => source,
        ObjectSource::Verified(None) => {
            return Err(UnityAssetError::format(format!(
                "Object {} bytes are unloaded and no verified SerializedFile source was supplied",
                info.path_id()
            )));
        }
    };
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
    Ok(ObjectData::BorrowedSourceRange {
        payload: source.payload,
        range: start..end,
    })
}

pub(crate) fn validate_source_binding(
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
    let file_backing = file.data_shared();
    if let Some(backing) = file_backing.as_arc_slice()
        && source.payload.shares_shared_backing(backing)
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

fn validate_file_state(file: &SerializedFile) -> Result<SerializedFileFormat> {
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
    Ok(format)
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
