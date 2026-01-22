use crate::Result;
use crate::binary_writer::BinaryWriter;
use unity_asset_binary::typetree::{TypeTree, TypeTreeNode};
use unity_asset_core::UnityAssetError;

/// Dump a TypeTree in the "blob" layout (Unity version >= 12 or version == 10).
///
/// Our parser stores `type_str_offset` / `name_str_offset` and `string_buffer` already, so we can
/// write the blob without rebuilding string tables.
pub fn dump_typetree_blob(tree: &TypeTree, writer: &mut BinaryWriter, version: u32) -> Result<()> {
    let mut flat = Vec::new();
    for root in &tree.nodes {
        flatten_preorder(root, &mut flat);
    }

    let node_count_i32: i32 = flat.len().try_into().map_err(|_| {
        UnityAssetError::format(format!("TypeTree node count too large: {}", flat.len()))
    })?;
    let buf_len_i32: i32 = tree.string_buffer.len().try_into().map_err(|_| {
        UnityAssetError::format(format!(
            "TypeTree string buffer too large: {}",
            tree.string_buffer.len()
        ))
    })?;

    writer.write_i32(node_count_i32);
    writer.write_i32(buf_len_i32);

    for node in flat {
        // Matches `TypeTreeParser::from_reader_blob` field widths.
        let v: u16 = node.version.try_into().unwrap_or(0);
        let level: u8 = node.level.try_into().unwrap_or(0);
        let type_flags: u8 = node.type_flags.try_into().unwrap_or(0);

        writer.write_u16(v);
        writer.write_u8(level);
        writer.write_u8(type_flags);
        writer.write_u32(node.type_str_offset);
        writer.write_u32(node.name_str_offset);
        writer.write_i32(node.byte_size);
        writer.write_i32(node.index);
        writer.write_i32(node.meta_flags);

        if version >= 19 {
            writer.write_u64(node.ref_type_hash);
        }
    }

    writer.write(tree.string_buffer.as_slice());
    Ok(())
}

/// Dump a TypeTree in the legacy "stringful" layout (Unity version < 12 and != 10).
///
/// Note: this is a best-effort implementation aligned with UnityPy's `TypeTreeNode.dump`.
pub fn dump_typetree_legacy(
    tree: &TypeTree,
    writer: &mut BinaryWriter,
    version: u32,
) -> Result<()> {
    // UnityPy always dumps the "node" (TypeTreeNode root) for a given type.
    let root = tree
        .nodes
        .first()
        .ok_or_else(|| UnityAssetError::format("Empty TypeTree"))?;
    dump_node_legacy(root, writer, version)?;
    Ok(())
}

fn dump_node_legacy(node: &TypeTreeNode, writer: &mut BinaryWriter, version: u32) -> Result<()> {
    // UnityPy uses an iterative stack; our recursive traversal is equivalent.
    writer.write_string_to_null(&node.type_name);
    writer.write_string_to_null(&node.name);
    writer.write_i32(node.byte_size);

    // NOTE: UnityPy has `m_VariableCount` for version==2, which we don't model. This repo mainly
    // targets Unity 5+; if needed, add it to the TypeTreeNode struct and parser.
    if version == 2 {
        return Err(UnityAssetError::format(
            "TypeTree legacy dump for version==2 is not supported yet",
        ));
    }

    if version != 3 {
        writer.write_i32(node.index);
    }
    writer.write_i32(node.type_flags);
    writer.write_i32(node.version);
    if version != 3 {
        writer.write_i32(node.meta_flags);
    }

    let child_count_i32: i32 = node.children.len().try_into().map_err(|_| {
        UnityAssetError::format(format!(
            "TypeTree child count too large: {}",
            node.children.len()
        ))
    })?;
    writer.write_i32(child_count_i32);

    for child in &node.children {
        dump_node_legacy(child, writer, version)?;
    }

    Ok(())
}

fn flatten_preorder<'a>(node: &'a TypeTreeNode, out: &mut Vec<&'a TypeTreeNode>) {
    out.push(node);
    for child in &node.children {
        flatten_preorder(child, out);
    }
}
