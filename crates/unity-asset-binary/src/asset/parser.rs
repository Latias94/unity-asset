//! SerializedFile parsing entry points and wire decoder.

use super::format::{
    MetadataField, ObjectOffsetEncoding, ObjectTailEncoding, ObjectTypeEncoding, PathIdEncoding,
    SerializedFileFormat,
};
use super::header::SerializedFileHeader;
use super::object_type_resolver::ObjectTypeResolver;
use super::serialized_file::ParsedParts;
pub use super::serialized_file::{FileStatistics, SerializedFile};
use super::types::{
    FileIdentifier, LocalSerializedObjectIdentifier, ObjectInfo, ObjectMetadata,
    ObjectTypeReference, SerializedType,
};
use super::validation;
use crate::data_view::DataView;
use crate::error::{BinaryError, Result};
use crate::random_access::{ByteCursor, ByteSource, SegmentedBytes};
use crate::reader::{BinaryInput, BinaryReader, ByteOrder, not_enough_data_u64};
use crate::shared_bytes::SharedBytes;
use std::ops::Range;
use unity_asset_core::AssetLoadBudget;

/// SerializedFile parser.
///
/// This parser supports contiguous and segmented input while charging a caller-owned load budget.
pub struct SerializedFileParser;

#[derive(Debug, Default)]
struct ParsedMetadata {
    unity_version: String,
    target_platform: i32,
    enable_type_tree: bool,
    types: Vec<SerializedType>,
    legacy_big_id: Option<i32>,
    objects: Vec<ObjectInfo>,
    script_types: Vec<LocalSerializedObjectIdentifier>,
    externals: Vec<FileIdentifier>,
    ref_types: Vec<SerializedType>,
    user_information: String,
}

impl SerializedFileParser {
    /// Parse a SerializedFile from owned binary data.
    pub fn from_bytes(data: Vec<u8>) -> Result<SerializedFile> {
        Self::from_bytes_with_options(data, false)
    }

    /// Parse a SerializedFile while charging a caller-owned load budget.
    pub fn from_bytes_with_budget(
        data: Vec<u8>,
        budget: &mut AssetLoadBudget,
    ) -> Result<SerializedFile> {
        Self::from_bytes_with_options_and_budget(data, false, budget)
    }

    /// Parse a SerializedFile from owned binary data with explicit preload behavior.
    pub fn from_bytes_with_options(
        data: Vec<u8>,
        preload_object_data: bool,
    ) -> Result<SerializedFile> {
        let mut budget = AssetLoadBudget::default();
        Self::from_bytes_with_options_and_budget(data, preload_object_data, &mut budget)
    }

    /// Parse owned binary data with explicit preload behavior and a caller-owned load budget.
    pub fn from_bytes_with_options_and_budget(
        data: Vec<u8>,
        preload_object_data: bool,
        budget: &mut AssetLoadBudget,
    ) -> Result<SerializedFile> {
        let shared = SharedBytes::from_vec(data);
        let len = shared.len();
        Self::from_shared_range_with_options_and_budget(shared, 0..len, preload_object_data, budget)
    }

    /// Parse a SerializedFile from a shared backing buffer and byte range.
    pub fn from_shared_range(data: SharedBytes, range: Range<usize>) -> Result<SerializedFile> {
        Self::from_shared_range_with_options(data, range, false)
    }

    /// Parse a shared byte range while charging a caller-owned load budget.
    pub fn from_shared_range_with_budget(
        data: SharedBytes,
        range: Range<usize>,
        budget: &mut AssetLoadBudget,
    ) -> Result<SerializedFile> {
        Self::from_shared_range_with_options_and_budget(data, range, false, budget)
    }

    /// Parse a shared byte range with explicit preload behavior.
    pub fn from_shared_range_with_options(
        data: SharedBytes,
        range: Range<usize>,
        preload_object_data: bool,
    ) -> Result<SerializedFile> {
        let mut budget = AssetLoadBudget::default();
        Self::from_shared_range_with_options_and_budget(
            data,
            range,
            preload_object_data,
            &mut budget,
        )
    }

    /// Parse a shared byte range with explicit preload behavior and a caller-owned budget.
    pub fn from_shared_range_with_options_and_budget(
        data: SharedBytes,
        range: Range<usize>,
        preload_object_data: bool,
        budget: &mut AssetLoadBudget,
    ) -> Result<SerializedFile> {
        let view = DataView::from_shared_range(data, range)?;
        Self::from_view_with_options_and_budget(view, preload_object_data, budget)
    }

    fn from_view_with_options_and_budget(
        view: DataView,
        preload_object_data: bool,
        budget: &mut AssetLoadBudget,
    ) -> Result<SerializedFile> {
        let parts = Self::parse_source(&view, budget)?;
        let mut file = SerializedFile::from_parsed_parts(parts, view)?;
        if preload_object_data {
            file.load_object_data(budget)?;
        }
        Ok(file)
    }

    /// Validate a segmented SerializedFile without materializing a contiguous image.
    #[doc(hidden)]
    pub fn validate_segmented_with_budget(
        image: &SegmentedBytes,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        Self::parse_source(image, budget).map(|_| ())
    }

    fn parse_source(source: &dyn ByteSource, budget: &mut AssetLoadBudget) -> Result<ParsedParts> {
        let header = {
            let mut input = ByteCursor::new(source, ByteOrder::Big, budget)?;
            SerializedFileHeader::from_input(&mut input)?
        };
        let format = SerializedFileFormat::new(header.version)?;
        let source_len = source.len();
        let regions = format.decode_regions(
            header.metadata_size,
            header.file_size,
            header.data_offset,
            source_len,
        )?;
        let metadata = {
            let mut input = ByteCursor::with_range(
                source,
                regions.metadata_body.clone(),
                header.byte_order(),
                budget,
            )?;
            let metadata =
                Self::parse_metadata(format, header.data_offset, &regions.data, &mut input)?;
            if input.remaining() != 0 {
                return Err(BinaryError::invalid_data(format!(
                    "SerializedFile metadata has {} unconsumed bytes",
                    input.remaining()
                )));
            }
            metadata
        };

        let parts = ParsedParts {
            format,
            regions,
            header,
            unity_version: metadata.unity_version,
            target_platform: metadata.target_platform,
            enable_type_tree: metadata.enable_type_tree,
            types: metadata.types,
            legacy_big_id: metadata.legacy_big_id,
            objects: metadata.objects,
            script_types: metadata.script_types,
            externals: metadata.externals,
            ref_types: metadata.ref_types,
            user_information: metadata.user_information,
        };
        validation::validate_parts(&parts, source_len)?;
        Ok(parts)
    }

    fn parse_metadata(
        format: SerializedFileFormat,
        data_offset: u64,
        data_region: &Range<u64>,
        input: &mut (impl BinaryInput + ?Sized),
    ) -> Result<ParsedMetadata> {
        let mut metadata = ParsedMetadata::default();
        if format.has_metadata_field(MetadataField::UnityVersion) {
            metadata.unity_version =
                input.read_cstring_limited(BinaryReader::DEFAULT_MAX_STRING_LEN)?;
        }

        if format.has_metadata_field(MetadataField::TargetPlatform) {
            metadata.target_platform = input.read_i32()?;
        }

        let explicit_tree_flag = format
            .has_metadata_field(MetadataField::EnableTypeTree)
            .then(|| input.read_bool())
            .transpose()?;
        metadata.enable_type_tree = format
            .type_tree_enablement()
            .resolve(explicit_tree_flag)
            .ok_or_else(|| BinaryError::invalid_data("Missing explicit enableTypeTree flag"))?;

        let type_count = read_table_count(input, "SerializedType", 4)?;
        reserve_entries(&mut metadata.types, type_count, "SerializedType")?;
        for _ in 0..type_count {
            metadata.types.push(SerializedType::from_input(
                input,
                format,
                metadata.enable_type_tree,
                false,
            )?);
        }

        if format.has_metadata_field(MetadataField::BigIdEnabled) {
            metadata.legacy_big_id = Some(input.read_i32()?);
        }
        let uses_big_ids = metadata.legacy_big_id.is_some_and(|value| value != 0);

        let object_count = read_table_count(
            input,
            "object",
            minimum_object_record_size(format, uses_big_ids),
        )?;
        let type_resolver =
            ObjectTypeResolver::new(format.object_type_encoding(), &metadata.types)?;
        reserve_entries(&mut metadata.objects, object_count, "object")?;
        for _ in 0..object_count {
            metadata.objects.push(Self::parse_object_info(
                format,
                uses_big_ids,
                data_offset,
                data_region,
                &type_resolver,
                input,
            )?);
        }

        if format.has_metadata_field(MetadataField::ScriptTypes) {
            let script_count = read_table_count(input, "script type", 8)?;
            reserve_entries(&mut metadata.script_types, script_count, "script type")?;
            for _ in 0..script_count {
                metadata
                    .script_types
                    .push(LocalSerializedObjectIdentifier::from_input(input, format)?);
            }
        }

        let external_count = read_table_count(input, "external", 1)?;
        reserve_entries(&mut metadata.externals, external_count, "external")?;
        for _ in 0..external_count {
            metadata
                .externals
                .push(FileIdentifier::from_input(input, format)?);
        }

        if format.has_metadata_field(MetadataField::RefTypes) {
            let ref_type_count = read_table_count(input, "reference type", 4)?;
            reserve_entries(&mut metadata.ref_types, ref_type_count, "reference type")?;
            for _ in 0..ref_type_count {
                metadata.ref_types.push(SerializedType::from_input(
                    input,
                    format,
                    metadata.enable_type_tree,
                    true,
                )?);
            }
        }

        if format.has_metadata_field(MetadataField::UserInformation) {
            metadata.user_information =
                input.read_cstring_limited(BinaryReader::DEFAULT_MAX_STRING_LEN)?;
        }

        Ok(metadata)
    }

    fn parse_object_info(
        format: SerializedFileFormat,
        uses_big_ids: bool,
        data_offset: u64,
        data_region: &Range<u64>,
        type_resolver: &ObjectTypeResolver<'_>,
        input: &mut (impl BinaryInput + ?Sized),
    ) -> Result<ObjectInfo> {
        let path_id = match format.path_id_encoding() {
            PathIdEncoding::I32 => i64::from(input.read_i32()?),
            PathIdEncoding::BigIdFlag if uses_big_ids => input.read_i64()?,
            PathIdEncoding::BigIdFlag => i64::from(input.read_i32()?),
            PathIdEncoding::AlignedI64 => {
                input.align()?;
                input.read_i64()?
            }
        };

        let relative_byte_start = match format.object_offset_encoding() {
            ObjectOffsetEncoding::U32 => u64::from(input.read_u32()?),
            ObjectOffsetEncoding::I64 => {
                i64_to_u64_checked(input.read_i64()?, "object.byte_start")?
            }
        };
        let byte_start = relative_byte_start
            .checked_add(data_offset)
            .ok_or_else(|| BinaryError::invalid_data("Object byte_start overflow"))?;
        let byte_size = input.read_u32()?;
        let byte_end = byte_start
            .checked_add(u64::from(byte_size))
            .ok_or_else(|| BinaryError::invalid_data("Object byte range overflow"))?;
        if byte_start < data_region.start || byte_end > data_region.end {
            return Err(BinaryError::invalid_data(format!(
                "Object path ID {path_id} range {byte_start}..{byte_end} is outside data region {}..{}",
                data_region.start, data_region.end
            )));
        }

        let raw_type_reference = input.read_i32()?;
        let type_reference = match format.object_type_encoding() {
            ObjectTypeEncoding::Legacy => ObjectTypeReference::Legacy {
                raw_type_id: raw_type_reference,
                class_id_bits: input.read_u16()?,
            },
            ObjectTypeEncoding::TransitionalV16 => ObjectTypeReference::TransitionalV16 {
                raw: raw_type_reference,
            },
            ObjectTypeEncoding::Indexed => {
                let index = u32::try_from(raw_type_reference).map_err(|_| {
                    BinaryError::invalid_data(format!(
                        "Negative SerializedType index in object table: {raw_type_reference}"
                    ))
                })?;
                ObjectTypeReference::SerializedTypeIndex { index }
            }
        };

        let metadata = match format.object_tail_encoding() {
            ObjectTailEncoding::Destroyed => ObjectMetadata::Destroyed {
                value: input.read_u16()?,
            },
            ObjectTailEncoding::ScriptTypeIndex => ObjectMetadata::ScriptTypeIndex {
                index: input.read_i16()?,
            },
            ObjectTailEncoding::ScriptTypeIndexAndStripped => {
                ObjectMetadata::ScriptTypeIndexAndStripped {
                    index: input.read_i16()?,
                    stripped: input.read_u8()?,
                }
            }
            ObjectTailEncoding::None => ObjectMetadata::None,
        };
        let (class_id, serialized_type_index) = type_resolver.resolve(type_reference, metadata)?;

        Ok(ObjectInfo::from_wire(
            path_id,
            byte_start,
            byte_size,
            type_reference,
            class_id,
            serialized_type_index,
            metadata,
        ))
    }
}

fn read_table_count(
    input: &mut (impl BinaryInput + ?Sized),
    label: &str,
    minimum_entry_size: usize,
) -> Result<usize> {
    let raw_count = input.read_i32()?;
    let count = u64::try_from(raw_count)
        .map_err(|_| BinaryError::invalid_data(format!("Negative {label} count: {raw_count}")))?;
    let minimum_entry_size = u64::try_from(minimum_entry_size)
        .map_err(|_| BinaryError::invalid_data("minimum entry size does not fit in u64"))?;
    let minimum_bytes = count.checked_mul(minimum_entry_size).ok_or_else(|| {
        BinaryError::ResourceLimitExceeded(format!("{label} table byte size overflow"))
    })?;
    if minimum_bytes > input.remaining() {
        return Err(not_enough_data_u64(minimum_bytes, input.remaining()));
    }
    input.consume_entries(count)?;
    usize::try_from(count)
        .map_err(|_| BinaryError::memory_error(format!("{label} count does not fit in usize")))
}

fn reserve_entries<T>(entries: &mut Vec<T>, count: usize, label: &str) -> Result<()> {
    entries.try_reserve_exact(count).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {count} {label} entries: {error}"
        ))
    })
}

fn minimum_object_record_size(format: SerializedFileFormat, uses_big_ids: bool) -> usize {
    let path_id_size = match format.path_id_encoding() {
        PathIdEncoding::I32 => 4,
        PathIdEncoding::BigIdFlag if uses_big_ids => 8,
        PathIdEncoding::BigIdFlag => 4,
        PathIdEncoding::AlignedI64 => 8,
    };
    let offset_size = match format.object_offset_encoding() {
        ObjectOffsetEncoding::U32 => 4,
        ObjectOffsetEncoding::I64 => 8,
    };
    let class_id_size = match format.object_type_encoding() {
        ObjectTypeEncoding::Legacy => 2,
        ObjectTypeEncoding::TransitionalV16 | ObjectTypeEncoding::Indexed => 0,
    };
    let tail_size = match format.object_tail_encoding() {
        ObjectTailEncoding::Destroyed | ObjectTailEncoding::ScriptTypeIndex => 2,
        ObjectTailEncoding::ScriptTypeIndexAndStripped => 3,
        ObjectTailEncoding::None => 0,
    };
    path_id_size + offset_size + 4 + 4 + class_id_size + tail_size
}

fn i64_to_u64_checked(value: i64, name: &'static str) -> Result<u64> {
    if value < 0 {
        return Err(BinaryError::invalid_data(format!(
            "Invalid {name}: negative value {value}"
        )));
    }
    Ok(value as u64)
}

#[cfg(test)]
#[path = "parser_tests.rs"]
mod tests;
