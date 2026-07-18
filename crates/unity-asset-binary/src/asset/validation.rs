//! Shared validation for parsed and writer-visible SerializedFile wire state.

use super::format::{SerializedFileFormat, SerializedFileRegions};
use super::header::SerializedFileHeader;
use super::object_type_resolver::ObjectTypeResolver;
use super::serialized_file::{ParsedParts, SerializedFile};
use super::types::{ObjectInfo, SerializedType};
use crate::error::{BinaryError, Result};
use crate::typetree::TypeTreeParser;
use std::collections::HashSet;

struct WireState<'a> {
    format: SerializedFileFormat,
    header: &'a SerializedFileHeader,
    regions: &'a SerializedFileRegions,
    enable_type_tree: bool,
    types: &'a [SerializedType],
    objects: &'a [ObjectInfo],
    ref_types: &'a [SerializedType],
    source_len: u64,
}

pub(super) fn validate_parts(parts: &ParsedParts, source_len: u64) -> Result<()> {
    validate_wire_state(WireState {
        format: parts.format,
        header: &parts.header,
        regions: &parts.regions,
        enable_type_tree: parts.enable_type_tree,
        types: &parts.types,
        objects: &parts.objects,
        ref_types: &parts.ref_types,
        source_len,
    })
}

pub(super) fn validate_file(file: &SerializedFile) -> Result<()> {
    let source_len = u64::try_from(file.data().len())
        .map_err(|_| BinaryError::invalid_data("SerializedFile length does not fit u64"))?;
    validate_wire_state(WireState {
        format: file.format(),
        header: &file.header,
        regions: file.regions(),
        enable_type_tree: file.type_tree_enabled(),
        types: file.types(),
        objects: file.objects(),
        ref_types: file.ref_types(),
        source_len,
    })
}

fn validate_wire_state(state: WireState<'_>) -> Result<()> {
    let decoded_regions = state.format.decode_regions(
        state.header.metadata_size,
        state.header.file_size,
        state.header.data_offset,
        state.source_len,
    )?;
    if decoded_regions != *state.regions {
        return Err(BinaryError::invalid_data(
            "SerializedFile cached regions do not match its header",
        ));
    }

    validate_type_table(
        state.types,
        state.format,
        state.enable_type_tree,
        "Type",
        "SerializedType",
    )?;
    validate_type_table(
        state.ref_types,
        state.format,
        state.enable_type_tree,
        "Reference type",
        "Reference type",
    )?;

    let type_resolver = ObjectTypeResolver::new(state.format.object_type_encoding(), state.types)?;
    let mut path_ids = HashSet::new();
    path_ids.try_reserve(state.objects.len()).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve object identity validation set: {error}"
        ))
    })?;
    for (index, object) in state.objects.iter().enumerate() {
        object.validate().map_err(|error| {
            BinaryError::invalid_data(format!("Object {index} validation failed: {error}"))
        })?;
        if !path_ids.insert(object.path_id()) {
            return Err(BinaryError::invalid_data(format!(
                "Duplicate object path ID {}",
                object.path_id()
            )));
        }
        let (class_id, serialized_type_index) = type_resolver
            .resolve(object.type_reference(), object.metadata())
            .map_err(|error| {
                BinaryError::invalid_data(format!(
                    "Object {index} type reference validation failed: {error}"
                ))
            })?;
        if object.class_id() != class_id || object.serialized_type_index() != serialized_type_index
        {
            return Err(BinaryError::invalid_data(format!(
                "Object {index} cached type resolution disagrees with its wire reference"
            )));
        }
    }

    Ok(())
}

fn validate_type_table(
    types: &[SerializedType],
    format: SerializedFileFormat,
    enable_type_tree: bool,
    validation_label: &str,
    tree_label: &str,
) -> Result<()> {
    for (index, serialized_type) in types.iter().enumerate() {
        serialized_type
            .validate_for_format(format)
            .map_err(|error| {
                BinaryError::invalid_data(format!(
                    "{validation_label} {index} validation failed: {error}"
                ))
            })?;
        if enable_type_tree {
            TypeTreeParser::validate_for_format(&serialized_type.type_tree, format).map_err(
                |error| {
                    BinaryError::invalid_data(format!(
                        "{tree_label} {index} has an invalid TypeTree: {error}"
                    ))
                },
            )?;
        }
    }
    Ok(())
}
