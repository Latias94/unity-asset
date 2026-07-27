use crate::Result;
use crate::serialized_file::sink::{EndianSink, SinkBackend};
use unity_asset_binary::asset::{SerializedFileFormat, TypeTreeEncoding};
use unity_asset_binary::typetree::{
    MAX_TYPE_TREE_DEPTH, MAX_TYPE_TREE_NODES, MAX_TYPE_TREE_STRING_BUFFER, TypeTree, TypeTreeNode,
};
use unity_asset_core::UnityAssetError;

pub(crate) fn dump_typetree_to<B: SinkBackend>(
    tree: &TypeTree,
    writer: &mut EndianSink<B>,
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

fn dump_blob<B: SinkBackend>(
    tree: &TypeTree,
    writer: &mut EndianSink<B>,
    format: SerializedFileFormat,
) -> Result<()> {
    let node_count = visit_preorder(&tree.nodes, |_| Ok(()))?;
    if tree.string_buffer.len() > MAX_TYPE_TREE_STRING_BUFFER {
        return Err(UnityAssetError::format(format!(
            "TypeTree string buffer size {} exceeds limit {MAX_TYPE_TREE_STRING_BUFFER}",
            tree.string_buffer.len()
        )));
    }

    let node_count = i32::try_from(node_count).map_err(|_| {
        UnityAssetError::format(format!("TypeTree node count too large: {node_count}"))
    })?;
    let string_buffer_size = i32::try_from(tree.string_buffer.len()).map_err(|_| {
        UnityAssetError::format(format!(
            "TypeTree string buffer too large: {}",
            tree.string_buffer.len()
        ))
    })?;

    writer.write_i32(node_count)?;
    writer.write_i32(string_buffer_size)?;

    visit_preorder(&tree.nodes, |node| write_blob_node(node, writer, format))?;

    writer.write(tree.string_buffer.as_slice())?;
    Ok(())
}

fn write_blob_node<B: SinkBackend>(
    node: &TypeTreeNode,
    writer: &mut EndianSink<B>,
    format: SerializedFileFormat,
) -> Result<()> {
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

    writer.write_i16(version)?;
    writer.write_u8(level)?;
    writer.write_u8(type_flags)?;
    writer.write_u32(node.type_str_offset)?;
    writer.write_u32(node.name_str_offset)?;
    writer.write_i32(node.byte_size)?;
    writer.write_i32(node.index)?;
    writer.write_i32(node.meta_flags)?;

    if matches!(
        format.type_tree_encoding(),
        TypeTreeEncoding::BlobWithRefTypeHash
    ) {
        writer.write_u64(node.ref_type_hash)?;
    }
    Ok(())
}

fn dump_legacy<B: SinkBackend>(
    tree: &TypeTree,
    writer: &mut EndianSink<B>,
    format: SerializedFileFormat,
) -> Result<()> {
    let [root] = tree.nodes.as_slice() else {
        return Err(UnityAssetError::format(format!(
            "Legacy TypeTree requires exactly one root, found {}",
            tree.nodes.len()
        )));
    };
    visit_preorder(std::slice::from_ref(root), |node| {
        write_legacy_node(node, writer, format.type_tree_encoding())
    })?;
    Ok(())
}

fn write_legacy_node<B: SinkBackend>(
    node: &TypeTreeNode,
    writer: &mut EndianSink<B>,
    encoding: TypeTreeEncoding,
) -> Result<()> {
    if node.type_name.len() > MAX_TYPE_TREE_STRING_BUFFER
        || node.name.len() > MAX_TYPE_TREE_STRING_BUFFER
    {
        return Err(UnityAssetError::format(format!(
            "Legacy TypeTree node string exceeds limit {MAX_TYPE_TREE_STRING_BUFFER}"
        )));
    }
    writer.write_string_to_null(&node.type_name)?;
    writer.write_string_to_null(&node.name)?;
    writer.write_i32(node.byte_size)?;

    if matches!(encoding, TypeTreeEncoding::LegacyV2) {
        writer.write_i32(node.variable_count)?;
    }

    if !matches!(encoding, TypeTreeEncoding::LegacyV3) {
        writer.write_i32(node.index)?;
    }
    writer.write_i32(node.type_flags)?;
    writer.write_i32(node.version)?;
    if !matches!(encoding, TypeTreeEncoding::LegacyV3) {
        writer.write_i32(node.meta_flags)?;
    }

    let child_count = i32::try_from(node.children.len()).map_err(|_| {
        UnityAssetError::format(format!(
            "TypeTree child count too large: {}",
            node.children.len()
        ))
    })?;
    writer.write_i32(child_count)?;

    Ok(())
}

fn visit_preorder<'tree>(
    roots: &'tree [TypeTreeNode],
    mut visit: impl FnMut(&'tree TypeTreeNode) -> Result<()>,
) -> Result<usize> {
    let mut levels: [Option<&'tree [TypeTreeNode]>; MAX_TYPE_TREE_DEPTH + 1] =
        [None; MAX_TYPE_TREE_DEPTH + 1];
    let mut next_indices = [0_usize; MAX_TYPE_TREE_DEPTH + 1];
    levels[0] = Some(roots);
    let mut depth = 0_usize;
    let mut node_count = 0_usize;

    loop {
        let nodes = levels[depth]
            .ok_or_else(|| UnityAssetError::format("TypeTree traversal lost its active depth"))?;
        let index = next_indices[depth];
        if index == nodes.len() {
            levels[depth] = None;
            next_indices[depth] = 0;
            if depth == 0 {
                break;
            }
            depth -= 1;
            continue;
        }
        next_indices[depth] = index + 1;
        let node = &nodes[index];
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
        if node_count == MAX_TYPE_TREE_NODES {
            return Err(UnityAssetError::format(format!(
                "TypeTree node count exceeds limit {MAX_TYPE_TREE_NODES}"
            )));
        }
        node_count += 1;
        visit(node)?;
        if !node.children.is_empty() {
            let child_depth = depth
                .checked_add(1)
                .ok_or_else(|| UnityAssetError::format("TypeTree depth overflow"))?;
            if child_depth > MAX_TYPE_TREE_DEPTH {
                return Err(UnityAssetError::format(format!(
                    "TypeTree depth {child_depth} exceeds limit {MAX_TYPE_TREE_DEPTH}"
                )));
            }
            levels[child_depth] = Some(&node.children);
            next_indices[child_depth] = 0;
            depth = child_depth;
        }
    }
    Ok(node_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BinaryWriter, ByteOrder};

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

        let mut writer = BinaryWriter::new(ByteOrder::Big);
        tree.version = 2;
        let byte_order = writer.byte_order();
        let mut sink = EndianSink::new(&mut writer, byte_order);
        dump_typetree_to(&tree, &mut sink, SerializedFileFormat::new(2).unwrap()).unwrap();
        let out = writer.into_result().unwrap();

        // Layout follows UnityPy TypeTreeNode.dump:
        // type\0, name\0, byte_size(i32), variable_count(i32), index(i32), ...
        assert!(out.starts_with(b"int\0m_Value\0"));
        let fixed = &out["int\0m_Value\0".len()..];
        assert_eq!(&fixed[0..4], &4i32.to_be_bytes()); // byte_size
        assert_eq!(&fixed[4..8], &123i32.to_be_bytes()); // variable_count
    }

    #[test]
    fn preorder_traversal_is_bounded_by_depth_instead_of_node_count() {
        let mut root = TypeTreeNode::new();
        root.level = 0;
        root.children = (0..10_000)
            .map(|_| {
                let mut child = TypeTreeNode::new();
                child.level = 1;
                child
            })
            .collect();

        let mut visited = 0_usize;
        let count = visit_preorder(std::slice::from_ref(&root), |_| {
            visited += 1;
            Ok(())
        })
        .unwrap();

        assert_eq!(count, 10_001);
        assert_eq!(visited, count);
    }

    #[test]
    fn preorder_traversal_accepts_the_maximum_structural_depth() {
        let mut node = TypeTreeNode::new();
        node.level = i32::try_from(MAX_TYPE_TREE_DEPTH).unwrap();
        for depth in (0..MAX_TYPE_TREE_DEPTH).rev() {
            let mut parent = TypeTreeNode::new();
            parent.level = i32::try_from(depth).unwrap();
            parent.children.push(node);
            node = parent;
        }

        assert_eq!(
            visit_preorder(std::slice::from_ref(&node), |_| Ok(())).unwrap(),
            MAX_TYPE_TREE_DEPTH + 1
        );
    }
}
