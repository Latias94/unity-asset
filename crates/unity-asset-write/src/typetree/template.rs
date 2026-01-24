use crate::binary_writer::{BinaryWriter, Endian};
use crate::typetree::context::TypeTreeWriteContext;
use crate::typetree::primitives::write_primitive;
use crate::typetree::referenced_object::write_referenced_object;
use crate::typetree::writer::{TypeTreeWriteOptions, write_value};
use crate::{Result, UnityAssetError};

use indexmap::IndexMap;
use unity_asset_binary::asset::SerializedType;
use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{TypeTree, TypeTreeNode, TypeTreeSerializer};
use unity_asset_core::UnityValue;

pub(crate) fn write_object_with_original_bytes(
    writer: &mut BinaryWriter,
    tree: &TypeTree,
    ref_types: Option<&[SerializedType]>,
    properties: &IndexMap<String, UnityValue>,
    original_bytes: &[u8],
    options: TypeTreeWriteOptions,
) -> Result<()> {
    let root = tree
        .nodes
        .first()
        .ok_or_else(|| UnityAssetError::format("TypeTreeWriter requires a non-empty TypeTree"))?;

    let byte_order = match writer.endian() {
        Endian::Big => ByteOrder::Big,
        Endian::Little => ByteOrder::Little,
    };

    let serializer = TypeTreeSerializer::new(tree);
    let mut original = BinaryReader::new(original_bytes, byte_order);
    let mut ctx = TypeTreeWriteContext {
        ref_types,
        ..Default::default()
    };

    for child in &root.children {
        let start = original.position() as usize;
        serializer
            .skip_value_with_ref_types(&mut original, child, ref_types)
            .map_err(|e| {
                UnityAssetError::with_source(
                    format!(
                        "Failed to scan original bytes for TypeTree template write (field='{}')",
                        child.name
                    ),
                    e,
                )
            })?;
        let end = original.position() as usize;
        let slice = original_bytes.get(start..end).ok_or_else(|| {
            UnityAssetError::format("TypeTree template scan produced invalid range")
        })?;

        if child.name.is_empty() {
            writer.write(slice);
            continue;
        }

        let Some(v) = properties.get(&child.name) else {
            if options.allow_missing_fields {
                writer.write(slice);
                continue;
            }
            return Err(UnityAssetError::format(format!(
                "Missing field '{}' for TypeTree write",
                child.name
            )));
        };

        write_value_with_template(
            writer,
            &serializer,
            ref_types,
            byte_order,
            child,
            v,
            slice,
            &mut ctx,
            options,
        )?;
    }

    Ok(())
}

fn write_value_with_template(
    writer: &mut BinaryWriter,
    serializer: &TypeTreeSerializer<'_>,
    ref_types: Option<&[SerializedType]>,
    byte_order: ByteOrder,
    node: &TypeTreeNode,
    value: &UnityValue,
    original_bytes: &[u8],
    ctx: &mut TypeTreeWriteContext<'_>,
    options: TypeTreeWriteOptions,
) -> Result<()> {
    let mut align = node.is_aligned();
    if node
        .children
        .iter()
        .any(|c| c.type_name == "Array" && c.is_aligned())
    {
        align = true;
    }

    if write_primitive(writer, node.type_name.as_str(), value)? {
        if align {
            writer.align_stream(4);
        }
        return Ok(());
    }

    if node.children.iter().any(|c| c.type_name == "Array") {
        write_array_with_template(
            writer,
            serializer,
            ref_types,
            byte_order,
            node,
            value,
            original_bytes,
            ctx,
            options,
        )?;
        if align {
            writer.align_stream(4);
        }
        return Ok(());
    }

    if node.type_name == "pair" && node.children.len() == 2 {
        let (v0, v1) = match value {
            UnityValue::Array(v) if v.len() == 2 => (&v[0], &v[1]),
            UnityValue::Object(m) => {
                let first = m.get("first").ok_or_else(|| {
                    UnityAssetError::format("TypeTree pair object missing 'first' field")
                })?;
                let second = m.get("second").ok_or_else(|| {
                    UnityAssetError::format("TypeTree pair object missing 'second' field")
                })?;
                (first, second)
            }
            _ => {
                return Err(UnityAssetError::format(format!(
                    "TypeTree write type mismatch: expected pair array/object for '{}', got {:?}",
                    node.name, value
                )));
            }
        };

        // If we have original bytes, split them for child-level template recursion.
        let (o0, o1) = if original_bytes.is_empty() {
            (&[][..], &[][..])
        } else {
            let mut original = BinaryReader::new(original_bytes, byte_order);
            let start0 = original.position() as usize;
            serializer
                .skip_value_with_ref_types(&mut original, &node.children[0], ref_types)
                .map_err(|e| UnityAssetError::with_source("Failed to scan pair child bytes", e))?;
            let end0 = original.position() as usize;
            let start1 = original.position() as usize;
            serializer
                .skip_value_with_ref_types(&mut original, &node.children[1], ref_types)
                .map_err(|e| UnityAssetError::with_source("Failed to scan pair child bytes", e))?;
            let end1 = original.position() as usize;

            (
                original_bytes.get(start0..end0).unwrap_or(&[]),
                original_bytes.get(start1..end1).unwrap_or(&[]),
            )
        };

        write_value_with_template(
            writer,
            serializer,
            ref_types,
            byte_order,
            &node.children[0],
            v0,
            o0,
            ctx,
            options,
        )?;
        write_value_with_template(
            writer,
            serializer,
            ref_types,
            byte_order,
            &node.children[1],
            v1,
            o1,
            ctx,
            options,
        )?;

        if align {
            writer.align_stream(4);
        }
        return Ok(());
    }

    if node.type_name == "ReferencedObject" {
        let referenced = value.as_object().ok_or_else(|| {
            UnityAssetError::format(format!(
                "TypeTree write type mismatch: expected object for ReferencedObject, got {:?}",
                value
            ))
        })?;
        write_referenced_object(referenced, writer, node, ctx, options)?;
        return Ok(());
    }

    // Preserve UnityPy behavior: skip extra ManagedReferencesRegistry nodes after the first.
    if node.type_name == "ManagedReferencesRegistry" {
        if ctx.has_managed_registry {
            return Ok(());
        }
        ctx.has_managed_registry = true;
    }

    if original_bytes.is_empty() {
        let requires_template =
            options.allow_missing_fields || node.children.iter().any(|c| c.name.is_empty());
        if requires_template {
            return Err(UnityAssetError::format(format!(
                "TypeTree write requires original bytes for template preservation (type='{}' name='{}')",
                node.type_name, node.name
            )));
        }
        write_value(writer, node, value, ctx, options)?;
        return Ok(());
    }

    let obj = match value {
        UnityValue::Object(m) => m,
        _ => {
            return Err(UnityAssetError::format(format!(
                "TypeTree write type mismatch: expected object for type '{}', got {:?}",
                node.type_name, value
            )));
        }
    };

    // Split the original slice into per-child segments so unnamed fields can be preserved.
    // For named fields, we still pass the original child slice down, enabling deeper preservation.
    let mut original = BinaryReader::new(original_bytes, byte_order);

    for child in &node.children {
        let start = original.position() as usize;
        serializer
            .skip_value_with_ref_types(&mut original, child, ref_types)
            .map_err(|e| {
                UnityAssetError::with_source(
                    format!(
                        "Failed to scan original bytes for TypeTree template write (field='{}')",
                        child.name
                    ),
                    e,
                )
            })?;
        let end = original.position() as usize;
        let slice = original_bytes.get(start..end).unwrap_or(&[]);

        if child.name.is_empty() {
            writer.write(slice);
            continue;
        }

        let Some(v) = obj.get(&child.name) else {
            if options.allow_missing_fields {
                writer.write(slice);
                continue;
            }
            return Err(UnityAssetError::format(format!(
                "Missing field '{}' for TypeTree write (parent type '{}')",
                child.name, node.type_name
            )));
        };

        write_value_with_template(
            writer, serializer, ref_types, byte_order, child, v, slice, ctx, options,
        )?;
    }

    if align {
        writer.align_stream(4);
    }
    Ok(())
}

fn write_array_with_template(
    writer: &mut BinaryWriter,
    serializer: &TypeTreeSerializer<'_>,
    ref_types: Option<&[SerializedType]>,
    byte_order: ByteOrder,
    node: &TypeTreeNode,
    value: &UnityValue,
    original_bytes: &[u8],
    ctx: &mut TypeTreeWriteContext<'_>,
    options: TypeTreeWriteOptions,
) -> Result<()> {
    let array_node = node
        .children
        .iter()
        .find(|c| c.type_name == "Array")
        .ok_or_else(|| UnityAssetError::format("TypeTree array node missing 'Array' child"))?;

    let elem_node = array_node.children.get(1).ok_or_else(|| {
        UnityAssetError::format("TypeTree array node missing element child at index 1")
    })?;

    if matches!(elem_node.type_name.as_str(), "UInt8" | "SInt8" | "char") {
        // Delegate to the normal writer logic; byte-like arrays have no nested fields to preserve.
        write_value(writer, node, value, ctx, options)?;
        return Ok(());
    }

    let elements = match value {
        UnityValue::Array(v) => v,
        _ => {
            return Err(UnityAssetError::format(format!(
                "TypeTree write type mismatch: expected array for '{}', got {:?}",
                node.name, value
            )));
        }
    };

    let len_i32: i32 = elements.len().try_into().map_err(|_| {
        UnityAssetError::format(format!(
            "Array too large for i32 length: {}",
            elements.len()
        ))
    })?;
    writer.write_i32(len_i32);

    // If we have original bytes, split them into per-element segments. Otherwise, pass empty slices
    // down; template recursion will fail only if an element actually needs preservation.
    let mut element_slices: Vec<&[u8]> = Vec::new();
    if !original_bytes.is_empty() {
        let mut original = BinaryReader::new(original_bytes, byte_order);
        let size_i32 = original.read_i32().map_err(|e| {
            UnityAssetError::with_source("Failed to read original array size for template write", e)
        })?;
        if size_i32 < 0 {
            return Err(UnityAssetError::format(format!(
                "Negative original array size while template-writing '{}': {}",
                node.name, size_i32
            )));
        }
        if let Some(size_node) = array_node.children.first()
            && size_node.is_aligned()
        {
            original.align_to(4).map_err(|e| {
                UnityAssetError::with_source("Failed to align original array reader", e)
            })?;
        }
        let original_len = size_i32 as usize;

        for _ in 0..original_len {
            let start = original.position() as usize;
            serializer
                .skip_value_with_ref_types(&mut original, elem_node, ref_types)
                .map_err(|e| {
                    UnityAssetError::with_source("Failed to scan original array element", e)
                })?;
            let end = original.position() as usize;
            element_slices.push(original_bytes.get(start..end).unwrap_or(&[]));
        }
    }

    for (idx, e) in elements.iter().enumerate() {
        let slice = element_slices.get(idx).copied().unwrap_or(&[]);
        write_value_with_template(
            writer, serializer, ref_types, byte_order, elem_node, e, slice, ctx, options,
        )?;
    }

    Ok(())
}
