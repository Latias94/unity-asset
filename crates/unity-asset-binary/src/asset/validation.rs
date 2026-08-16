//! Shared validation for parsed and writer-visible SerializedFile wire state.

use super::format::{SerializedFileFormat, SerializedFileRegions};
use super::header::SerializedFileHeader;
use super::object_type_resolver::ObjectTypeResolver;
use super::serialized_file::{ParsedParts, SerializedFile};
use super::types::{ObjectInfo, SerializedType};
use crate::error::{BinaryError, BinaryObjectIdentityError, Result};
use crate::typetree::TypeTreeParser;
use std::mem::size_of;
use unity_asset_core::{AssetLoadBudget, BudgetError};

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

pub(super) fn validate_parts(
    parts: &ParsedParts,
    source_len: u64,
    budget: &mut AssetLoadBudget,
) -> Result<()> {
    validate_wire_state(
        WireState {
            format: parts.format,
            header: &parts.header,
            regions: &parts.regions,
            enable_type_tree: parts.enable_type_tree,
            types: &parts.types,
            objects: &parts.objects,
            ref_types: &parts.ref_types,
            source_len,
        },
        Some(budget),
    )
}

pub(super) fn validate_file(file: &SerializedFile) -> Result<()> {
    let source_len = u64::try_from(file.data().len())
        .map_err(|_| BinaryError::invalid_data("SerializedFile length does not fit u64"))?;
    validate_wire_state(
        WireState {
            format: file.format(),
            header: &file.header,
            regions: file.regions(),
            enable_type_tree: file.type_tree_enabled(),
            types: file.types(),
            objects: file.objects(),
            ref_types: file.ref_types(),
            source_len,
        },
        None,
    )
}

fn validate_wire_state(
    state: WireState<'_>,
    identity_budget: Option<&mut AssetLoadBudget>,
) -> Result<()> {
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
    for (index, object) in state.objects.iter().enumerate() {
        if object.path_id() == 0 {
            return Err(BinaryObjectIdentityError::ZeroPathId.into());
        }
        object.validate().map_err(|error| {
            BinaryError::invalid_data(format!("Object {index} validation failed: {error}"))
        })?;
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

    if let Some(budget) = identity_budget {
        let mut path_ids = reserve_path_ids(state.objects.len(), budget)?;
        path_ids.extend(state.objects.iter().map(ObjectInfo::path_id));
        path_ids.sort_unstable();
        if let Some(path_id) = path_ids
            .windows(2)
            .find_map(|pair| (pair[0] == pair[1]).then_some(pair[0]))
        {
            return Err(BinaryObjectIdentityError::DuplicatePathId { path_id }.into());
        }
    }

    Ok(())
}

fn reserve_path_ids(count: usize, budget: &mut AssetLoadBudget) -> Result<Vec<i64>> {
    let count =
        u64::try_from(count).map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let entry_size = u64::try_from(size_of::<i64>())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let allocation = count
        .checked_mul(entry_size)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    budget.check_bytes(allocation)?;

    let capacity = usize::try_from(count)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let mut path_ids = Vec::new();
    path_ids.try_reserve_exact(capacity).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {capacity} object path IDs for validation: {error}"
        ))
    })?;
    budget.consume_bytes(allocation)?;
    Ok(path_ids)
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

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::AssetLoadLimits;

    #[test]
    fn path_id_scratch_checks_budget_before_reserving() {
        let expected_bytes = u64::try_from(2 * size_of::<i64>()).unwrap();
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: expected_bytes,
            ..Default::default()
        })
        .unwrap();
        let path_ids = reserve_path_ids(2, &mut exact).unwrap();
        assert_eq!(path_ids.capacity(), 2);
        assert_eq!(exact.usage().bytes, expected_bytes);

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: expected_bytes - 1,
            ..Default::default()
        })
        .unwrap();
        let error = reserve_path_ids(2, &mut short).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == expected_bytes - 1 && requested == expected_bytes
        ));
        assert_eq!(short.usage().bytes, 0);
    }

    #[test]
    fn path_id_scratch_overflow_remains_a_typed_budget_error() {
        let mut budget = AssetLoadBudget::default();
        let error = reserve_path_ids(usize::MAX, &mut budget).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::ArithmeticOverflow { resource: "bytes" })
        ));
        assert_eq!(budget.usage().bytes, 0);
    }
}
