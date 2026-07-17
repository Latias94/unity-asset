use crate::Result;
use crate::binary_writer::BinaryWriter;
use unity_asset_binary::asset::{SerializedFileFormat, TypeTreeEncoding};
use unity_asset_binary::typetree::{
    MAX_TYPE_TREE_DEPTH, MAX_TYPE_TREE_NODES, MAX_TYPE_TREE_STRING_BUFFER, TypeTree, TypeTreeNode,
};
use unity_asset_core::UnityAssetError;

pub fn dump_typetree(
    tree: &TypeTree,
    writer: &mut BinaryWriter,
    format: SerializedFileFormat,
) -> Result<()> {
    match format.type_tree_encoding() {
        TypeTreeEncoding::LegacyV2
        | TypeTreeEncoding::LegacyV3
        | TypeTreeEncoding::LegacyStandard => dump_legacy(tree, writer, format),
        TypeTreeEncoding::Blob | TypeTreeEncoding::BlobWithRefTypeHash => {
            dump_blob(tree, writer, format)
        }
    }
}

fn dump_blob(
    tree: &TypeTree,
    writer: &mut BinaryWriter,
    format: SerializedFileFormat,
) -> Result<()> {
    let flat = flatten_preorder(&tree.nodes)?;
    if tree.string_buffer.len() > MAX_TYPE_TREE_STRING_BUFFER {
        return Err(UnityAssetError::format(format!(
            "TypeTree string buffer size {} exceeds limit {MAX_TYPE_TREE_STRING_BUFFER}",
            tree.string_buffer.len()
        )));
    }

    let node_count = i32::try_from(flat.len()).map_err(|_| {
        UnityAssetError::format(format!("TypeTree node count too large: {}", flat.len()))
    })?;
    let string_buffer_size = i32::try_from(tree.string_buffer.len()).map_err(|_| {
        UnityAssetError::format(format!(
            "TypeTree string buffer too large: {}",
            tree.string_buffer.len()
        ))
    })?;

    writer.write_i32(node_count);
    writer.write_i32(string_buffer_size);

    for node in flat {
        let version = i16::try_from(node.version).map_err(|_| {
            UnityAssetError::format(format!(
                "TypeTree node version {} does not fit i16",
                node.version
            ))
        })?;
        let level = u8::try_from(node.level).map_err(|_| {
            UnityAssetError::format(format!(
                "TypeTree node level {} does not fit u8",
                node.level
            ))
        })?;
        let type_flags = u8::try_from(node.type_flags).map_err(|_| {
            UnityAssetError::format(format!(
                "TypeTree node flags {} do not fit u8",
                node.type_flags
            ))
        })?;

        writer.write_i16(version);
        writer.write_u8(level);
        writer.write_u8(type_flags);
        writer.write_u32(node.type_str_offset);
        writer.write_u32(node.name_str_offset);
        writer.write_i32(node.byte_size);
        writer.write_i32(node.index);
        writer.write_i32(node.meta_flags);

        if matches!(
            format.type_tree_encoding(),
            TypeTreeEncoding::BlobWithRefTypeHash
        ) {
            writer.write_u64(node.ref_type_hash);
        }
    }

    writer.write(tree.string_buffer.as_slice());
    Ok(())
}

fn dump_legacy(
    tree: &TypeTree,
    writer: &mut BinaryWriter,
    format: SerializedFileFormat,
) -> Result<()> {
    let [root] = tree.nodes.as_slice() else {
        return Err(UnityAssetError::format(format!(
            "Legacy TypeTree requires exactly one root, found {}",
            tree.nodes.len()
        )));
    };
    for node in flatten_preorder(std::slice::from_ref(root))? {
        write_legacy_node(node, writer, format.type_tree_encoding())?;
    }
    Ok(())
}

fn write_legacy_node(
    node: &TypeTreeNode,
    writer: &mut BinaryWriter,
    encoding: TypeTreeEncoding,
) -> Result<()> {
    if node.type_name.len() > MAX_TYPE_TREE_STRING_BUFFER
        || node.name.len() > MAX_TYPE_TREE_STRING_BUFFER
    {
        return Err(UnityAssetError::format(format!(
            "Legacy TypeTree node string exceeds limit {MAX_TYPE_TREE_STRING_BUFFER}"
        )));
    }
    writer.write_string_to_null(&node.type_name);
    writer.write_string_to_null(&node.name);
    writer.write_i32(node.byte_size);

    if matches!(encoding, TypeTreeEncoding::LegacyV2) {
        writer.write_i32(node.variable_count);
    }

    if !matches!(encoding, TypeTreeEncoding::LegacyV3) {
        writer.write_i32(node.index);
    }
    writer.write_i32(node.type_flags);
    writer.write_i32(node.version);
    if !matches!(encoding, TypeTreeEncoding::LegacyV3) {
        writer.write_i32(node.meta_flags);
    }

    let child_count = i32::try_from(node.children.len()).map_err(|_| {
        UnityAssetError::format(format!(
            "TypeTree child count too large: {}",
            node.children.len()
        ))
    })?;
    writer.write_i32(child_count);

    Ok(())
}

fn flatten_preorder(roots: &[TypeTreeNode]) -> Result<Vec<&TypeTreeNode>> {
    let mut output = Vec::new();
    let mut stack = Vec::new();
    stack.try_reserve(roots.len()).map_err(|error| {
        UnityAssetError::format(format!(
            "Failed to reserve TypeTree traversal stack: {error}"
        ))
    })?;
    stack.extend(roots.iter().rev().map(|node| (node, 0usize)));
    while let Some((node, depth)) = stack.pop() {
        if depth > MAX_TYPE_TREE_DEPTH {
            return Err(UnityAssetError::format(format!(
                "TypeTree depth {depth} exceeds limit {MAX_TYPE_TREE_DEPTH}"
            )));
        }
        let wire_level = usize::try_from(node.level).map_err(|_| {
            UnityAssetError::format(format!("Negative TypeTree node level {}", node.level))
        })?;
        if wire_level != depth {
            return Err(UnityAssetError::format(format!(
                "TypeTree node level {wire_level} disagrees with structural depth {depth}"
            )));
        }
        if output.len() == MAX_TYPE_TREE_NODES {
            return Err(UnityAssetError::format(format!(
                "TypeTree node count exceeds limit {MAX_TYPE_TREE_NODES}"
            )));
        }
        output.try_reserve(1).map_err(|error| {
            UnityAssetError::format(format!("Failed to reserve flattened TypeTree: {error}"))
        })?;
        output.push(node);
        stack.try_reserve(node.children.len()).map_err(|error| {
            UnityAssetError::format(format!(
                "Failed to reserve TypeTree traversal stack: {error}"
            ))
        })?;
        let child_depth = depth
            .checked_add(1)
            .ok_or_else(|| UnityAssetError::format("TypeTree depth overflow"))?;
        stack.extend(node.children.iter().rev().map(|child| (child, child_depth)));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BinaryWriter, Endian};

    #[test]
    fn typetree_legacy_dump_v2_includes_variable_count() {
        let mut root = TypeTreeNode::new();
        root.type_name = "int".to_string();
        root.name = "m_Value".to_string();
        root.byte_size = 4;
        root.variable_count = 123;
        root.index = 0;
        root.type_flags = 0;
        root.version = 1;
        root.meta_flags = 0;
        root.level = 0;
        root.children = Vec::new();

        let mut tree = TypeTree::new();
        tree.nodes = vec![root];

        let mut writer = BinaryWriter::new(Endian::Big);
        tree.version = 2;
        dump_typetree(&tree, &mut writer, SerializedFileFormat::new(2).unwrap()).unwrap();
        let out = writer.into_result().unwrap();

        // Layout follows UnityPy TypeTreeNode.dump:
        // type\0, name\0, byte_size(i32), variable_count(i32), index(i32), ...
        assert!(out.starts_with(b"int\0m_Value\0"));
        let fixed = &out["int\0m_Value\0".len()..];
        assert_eq!(&fixed[0..4], &4i32.to_be_bytes()); // byte_size
        assert_eq!(&fixed[4..8], &123i32.to_be_bytes()); // variable_count
    }
}
