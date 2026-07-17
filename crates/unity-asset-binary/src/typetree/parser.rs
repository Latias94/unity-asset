//! TypeTree parser implementation
//!
//! This module provides parsing functionality for Unity TypeTree structures,
//! inspired by UnityPy/classes/TypeTree.py

use super::common_strings;
use super::types::{TypeTree, TypeTreeNode};
use crate::asset::format::{SerializedFileFormat, TypeTreeEncoding};
use crate::error::{BinaryError, Result};
use crate::random_access::{BorrowedBytes, ByteCursor};
use crate::reader::{BinaryInput, BinaryReader, ByteOrder, not_enough_data_u64};
use unity_asset_core::AssetLoadBudget;

pub const MAX_TYPE_TREE_NODES: usize = 1_000_000;
pub const MAX_TYPE_TREE_DEPTH: usize = 512;
pub const MAX_TYPE_TREE_STRING_BUFFER: usize = BinaryReader::DEFAULT_MAX_STRING_LEN;

/// TypeTree parser
///
/// This struct handles the parsing of TypeTree structures from binary data,
/// supporting different Unity versions and formats.
pub struct TypeTreeParser;

impl TypeTreeParser {
    /// Parses a TypeTree from bytes through a caller-owned cumulative load budget.
    pub fn from_bytes_with_format(
        data: &[u8],
        byte_order: ByteOrder,
        format: SerializedFileFormat,
        budget: &mut AssetLoadBudget,
    ) -> Result<TypeTree> {
        let source = BorrowedBytes::new(data);
        let mut input = ByteCursor::new(&source, byte_order, budget)?;
        Self::from_input_with_format(&mut input, format)
    }

    pub(crate) fn from_input_with_format<I: BinaryInput + ?Sized>(
        input: &mut I,
        format: SerializedFileFormat,
    ) -> Result<TypeTree> {
        match format.type_tree_encoding() {
            encoding @ (TypeTreeEncoding::LegacyV2
            | TypeTreeEncoding::LegacyV3
            | TypeTreeEncoding::LegacyStandard) => Self::read_legacy_tree(input, format, encoding),
            encoding @ (TypeTreeEncoding::Blob | TypeTreeEncoding::BlobWithRefTypeHash) => {
                Self::read_blob_tree(input, format, encoding)
            }
        }
    }

    /// Parses a TypeTree after validating the numeric SerializedFile version.
    pub fn from_bytes(
        data: &[u8],
        byte_order: ByteOrder,
        version: u32,
        budget: &mut AssetLoadBudget,
    ) -> Result<TypeTree> {
        Self::from_bytes_with_format(
            data,
            byte_order,
            SerializedFileFormat::new(version)?,
            budget,
        )
    }

    /// Parses a blob TypeTree after validating that the format uses blob encoding.
    pub fn from_blob_bytes(
        data: &[u8],
        byte_order: ByteOrder,
        version: u32,
        budget: &mut AssetLoadBudget,
    ) -> Result<TypeTree> {
        let format = SerializedFileFormat::new(version)?;
        let encoding = format.type_tree_encoding();
        if !matches!(
            encoding,
            TypeTreeEncoding::Blob | TypeTreeEncoding::BlobWithRefTypeHash
        ) {
            return Err(BinaryError::invalid_data(format!(
                "SerializedFile v{version} does not use blob TypeTree encoding"
            )));
        }
        let source = BorrowedBytes::new(data);
        let mut input = ByteCursor::new(&source, byte_order, budget)?;
        Self::read_blob_tree(&mut input, format, encoding)
    }

    fn read_legacy_tree<I: BinaryInput + ?Sized>(
        input: &mut I,
        format: SerializedFileFormat,
        encoding: TypeTreeEncoding,
    ) -> Result<TypeTree> {
        let mut tree = TypeTree::new();
        tree.version = format.version();
        let mut nodes_read = 0_u64;
        let root = Self::read_legacy_node(input, encoding, 0, &mut nodes_read)?;
        tree.nodes.try_reserve_exact(1).map_err(|error| {
            BinaryError::memory_error(format!("Failed to reserve legacy TypeTree root: {error}"))
        })?;
        tree.nodes.push(root);
        Ok(tree)
    }

    fn read_legacy_node<I: BinaryInput + ?Sized>(
        input: &mut I,
        encoding: TypeTreeEncoding,
        depth: u32,
        nodes_read: &mut u64,
    ) -> Result<TypeTreeNode> {
        if u64::from(depth) > MAX_TYPE_TREE_DEPTH as u64 {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "TypeTree depth {depth} exceeds limit {MAX_TYPE_TREE_DEPTH}"
            )));
        }
        input.observe_depth(depth)?;
        *nodes_read = (*nodes_read).checked_add(1).ok_or_else(|| {
            BinaryError::ResourceLimitExceeded("TypeTree node count overflow".into())
        })?;
        if *nodes_read > MAX_TYPE_TREE_NODES as u64 {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "TypeTree node count exceeds limit {MAX_TYPE_TREE_NODES}"
            )));
        }
        input.consume_entries(1)?;

        let mut node = TypeTreeNode::new();
        node.level = i32::try_from(depth)
            .map_err(|_| BinaryError::invalid_data("TypeTree depth does not fit i32"))?;
        node.type_name = read_cstring_limited(input, "legacy TypeTree type")?;
        node.name = read_cstring_limited(input, "legacy TypeTree name")?;
        node.byte_size = input.read_i32()?;
        if matches!(encoding, TypeTreeEncoding::LegacyV2) {
            node.variable_count = input.read_i32()?;
        }
        if !matches!(encoding, TypeTreeEncoding::LegacyV3) {
            node.index = input.read_i32()?;
        }
        node.type_flags = input.read_i32()?;
        node.version = input.read_i32()?;
        if !matches!(encoding, TypeTreeEncoding::LegacyV3) {
            node.meta_flags = input.read_i32()?;
        }

        let child_count = read_non_negative_count(input, "legacy TypeTree child")?;
        let min_node_size = legacy_min_node_size(encoding);
        ensure_count_fits_remaining(input, child_count, min_node_size, "legacy TypeTree child")?;
        input.consume_members(child_count)?;
        let child_count = count_to_usize(child_count, "legacy TypeTree child")?;
        node.children
            .try_reserve_exact(child_count)
            .map_err(|error| {
                BinaryError::memory_error(format!(
                    "Failed to reserve {child_count} legacy TypeTree children: {error}"
                ))
            })?;
        for _ in 0..child_count {
            let child_depth = depth
                .checked_add(1)
                .ok_or_else(|| BinaryError::invalid_data("TypeTree depth overflows u32"))?;
            node.children.push(Self::read_legacy_node(
                input,
                encoding,
                child_depth,
                nodes_read,
            )?);
        }
        Ok(node)
    }

    fn read_blob_tree<I: BinaryInput + ?Sized>(
        input: &mut I,
        format: SerializedFileFormat,
        encoding: TypeTreeEncoding,
    ) -> Result<TypeTree> {
        let mut tree = TypeTree::new();
        tree.version = format.version();

        let node_count = read_non_negative_count(input, "TypeTree node")?;
        let string_buffer_size = read_non_negative_count(input, "TypeTree string buffer")?;
        if node_count > MAX_TYPE_TREE_NODES as u64 {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "TypeTree node count {node_count} exceeds limit {MAX_TYPE_TREE_NODES}"
            )));
        }
        if string_buffer_size > MAX_TYPE_TREE_STRING_BUFFER as u64 {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "TypeTree string buffer size {string_buffer_size} exceeds limit {MAX_TYPE_TREE_STRING_BUFFER}"
            )));
        }
        let node_width = if matches!(encoding, TypeTreeEncoding::BlobWithRefTypeHash) {
            32_u64
        } else {
            24_u64
        };
        let node_bytes = node_count
            .checked_mul(node_width)
            .ok_or_else(|| BinaryError::invalid_data("TypeTree node table size overflow"))?;
        let required = node_bytes
            .checked_add(string_buffer_size)
            .ok_or_else(|| BinaryError::invalid_data("TypeTree payload size overflow"))?;
        if required > input.remaining() {
            return Err(not_enough_data_u64(required, input.remaining()));
        }
        input.consume_entries(node_count)?;
        let node_count = count_to_usize(node_count, "TypeTree node")?;
        let string_buffer_size = count_to_usize(string_buffer_size, "TypeTree string buffer")?;
        tree.nodes.try_reserve_exact(node_count).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {node_count} TypeTree nodes: {error}"
            ))
        })?;
        for _ in 0..node_count {
            let mut node = TypeTreeNode::new();
            node.version = i32::from(input.read_i16()?);
            let level = input.read_u8()?;
            input.observe_depth(u32::from(level))?;
            node.level = i32::from(level);
            node.type_flags = i32::from(input.read_u8()?);
            node.type_str_offset = input.read_u32()?;
            node.name_str_offset = input.read_u32()?;
            node.byte_size = input.read_i32()?;
            node.index = input.read_i32()?;
            node.meta_flags = input.read_i32()?;

            if matches!(encoding, TypeTreeEncoding::BlobWithRefTypeHash) {
                node.ref_type_hash = input.read_u64()?;
            }
            tree.nodes.push(node);
        }
        tree.string_buffer = input.read_bytes(string_buffer_size)?;
        Self::resolve_strings(&mut tree, input)?;
        Self::build_hierarchy(&mut tree, input)?;
        Ok(tree)
    }

    /// Resolve string references in the TypeTree
    fn resolve_strings(tree: &mut TypeTree, input: &mut (impl BinaryInput + ?Sized)) -> Result<()> {
        let owned_bytes = tree.nodes.iter().try_fold(0_u64, |total, node| {
            let type_name = Self::resolve_string(&tree.string_buffer, node.type_str_offset)?;
            let field_name = Self::resolve_string(&tree.string_buffer, node.name_str_offset)?;
            let node_bytes = u64::try_from(type_name.len())
                .ok()
                .and_then(|type_bytes| {
                    u64::try_from(field_name.len())
                        .ok()
                        .and_then(|name_bytes| type_bytes.checked_add(name_bytes))
                })
                .ok_or_else(|| BinaryError::memory_error("TypeTree owned string size overflow"))?;
            total
                .checked_add(node_bytes)
                .ok_or_else(|| BinaryError::memory_error("TypeTree owned string total overflow"))
        })?;
        input.consume_bytes(owned_bytes)?;

        for node in &mut tree.nodes {
            Self::resolve_node_strings(node, &tree.string_buffer)?;
        }
        Ok(())
    }

    /// Resolve string references for a single node and its children
    fn resolve_node_strings(node: &mut TypeTreeNode, string_buffer: &[u8]) -> Result<()> {
        // Resolve type name
        node.type_name = copy_string(Self::resolve_string(string_buffer, node.type_str_offset)?)?;

        // Resolve field name
        node.name = copy_string(Self::resolve_string(string_buffer, node.name_str_offset)?)?;

        // Resolve children
        for child in &mut node.children {
            Self::resolve_node_strings(child, string_buffer)?;
        }

        Ok(())
    }

    /// Resolve TypeTree strings which can either reference the local string buffer or a global
    /// common string buffer (signaled via the high bit in blob TypeTrees).
    fn resolve_string(buffer: &[u8], offset: u32) -> Result<&str> {
        const COMMON_STRING_FLAG: u32 = 0x8000_0000;

        if (offset & COMMON_STRING_FLAG) != 0 {
            let common_offset = offset & !COMMON_STRING_FLAG;
            return common_strings::get_common_string(common_offset).ok_or_else(|| {
                BinaryError::invalid_data(format!(
                    "Unknown TypeTree common-string offset {common_offset}"
                ))
            });
        }

        Self::get_string_from_buffer(buffer, offset)
    }

    /// Get string from buffer at offset
    fn get_string_from_buffer(buffer: &[u8], offset: u32) -> Result<&str> {
        let start = usize::try_from(offset)
            .map_err(|_| BinaryError::invalid_data("TypeTree string offset does not fit usize"))?;
        if start >= buffer.len() {
            return Err(BinaryError::invalid_data(format!(
                "TypeTree local string offset {offset} is outside buffer length {}",
                buffer.len()
            )));
        }
        if start != 0 && buffer[start - 1] != 0 {
            return Err(BinaryError::invalid_data(format!(
                "TypeTree local string offset {offset} does not point to a string start"
            )));
        }
        let end = buffer[start..]
            .iter()
            .position(|&b| b == 0)
            .map(|pos| start + pos)
            .ok_or_else(|| {
                BinaryError::invalid_data(format!(
                    "TypeTree string at local offset {offset} has no null terminator"
                ))
            })?;

        std::str::from_utf8(&buffer[start..end]).map_err(|error| {
            BinaryError::invalid_data(format!(
                "Invalid UTF-8 at TypeTree string offset {offset}: {error}"
            ))
        })
    }

    /// Build hierarchical structure from flat node list
    fn build_hierarchy(tree: &mut TypeTree, input: &mut (impl BinaryInput + ?Sized)) -> Result<()> {
        if tree.nodes.is_empty() {
            return Ok(());
        }

        let member_count = tree.nodes.iter().try_fold(0_u64, |count, node| {
            let level = u32::try_from(node.level).map_err(|_| {
                BinaryError::invalid_data(format!("Negative TypeTree node level {}", node.level))
            })?;
            input.observe_depth(level)?;
            count
                .checked_add(u64::from(level > 0))
                .ok_or_else(|| BinaryError::invalid_data("TypeTree member count overflow"))
        })?;
        input.consume_members(member_count)?;

        let flat_nodes = std::mem::take(&mut tree.nodes);
        let mut stack: Vec<TypeTreeNode> = Vec::new();
        let stack_capacity = flat_nodes.len().min(MAX_TYPE_TREE_DEPTH + 1);
        stack.try_reserve_exact(stack_capacity).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve TypeTree hierarchy stack: {error}"
            ))
        })?;
        let mut roots = Vec::new();

        for mut node in flat_nodes {
            let level = usize::try_from(node.level).map_err(|_| {
                BinaryError::invalid_data(format!("Negative TypeTree node level {}", node.level))
            })?;
            if level > MAX_TYPE_TREE_DEPTH {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "TypeTree node level {level} exceeds limit {MAX_TYPE_TREE_DEPTH}"
                )));
            }
            while stack.len() > level {
                close_hierarchy_node(&mut stack, &mut roots)?;
            }
            if stack.len() != level {
                return Err(BinaryError::invalid_data(format!(
                    "TypeTree node level jumps from {} to {level}",
                    stack.len().saturating_sub(1)
                )));
            }
            node.children.clear();
            stack.push(node);
        }
        while !stack.is_empty() {
            close_hierarchy_node(&mut stack, &mut roots)?;
        }

        tree.nodes = roots;
        Ok(())
    }

    /// Validate parsed TypeTree
    pub fn validate(tree: &TypeTree) -> Result<()> {
        if tree.nodes.is_empty() {
            return Err(BinaryError::invalid_data("TypeTree has no nodes"));
        }

        for (i, node) in tree.nodes.iter().enumerate() {
            Self::validate_node(node, 0).map_err(|e| {
                BinaryError::generic(format!("Node {} validation failed: {}", i, e))
            })?;
        }

        Ok(())
    }

    /// Validates semantic and raw fields against a concrete SerializedFile encoding.
    pub fn validate_for_format(tree: &TypeTree, format: SerializedFileFormat) -> Result<()> {
        Self::validate(tree)?;
        let encoding = format.type_tree_encoding();
        if matches!(
            encoding,
            TypeTreeEncoding::LegacyV2
                | TypeTreeEncoding::LegacyV3
                | TypeTreeEncoding::LegacyStandard
        ) && !tree.string_buffer.is_empty()
        {
            return Err(BinaryError::invalid_data(format!(
                "SerializedFile v{} legacy TypeTree cannot encode a string buffer",
                format.version()
            )));
        }

        let mut stack = Vec::new();
        stack.try_reserve(tree.nodes.len()).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve TypeTree validation stack: {error}"
            ))
        })?;
        stack.extend(tree.nodes.iter().rev());
        while let Some(node) = stack.pop() {
            match encoding {
                TypeTreeEncoding::LegacyV2 => {
                    validate_legacy_only_fields(node, format, true)?;
                }
                TypeTreeEncoding::LegacyV3 | TypeTreeEncoding::LegacyStandard => {
                    validate_legacy_only_fields(node, format, false)?;
                }
                TypeTreeEncoding::Blob | TypeTreeEncoding::BlobWithRefTypeHash => {
                    if node.variable_count != 0 {
                        return Err(BinaryError::invalid_data(format!(
                            "SerializedFile v{} blob TypeTree cannot encode variable_count {}",
                            format.version(),
                            node.variable_count
                        )));
                    }
                    if matches!(encoding, TypeTreeEncoding::Blob) && node.ref_type_hash != 0 {
                        return Err(BinaryError::invalid_data(format!(
                            "SerializedFile v{} TypeTree cannot encode ref_type_hash",
                            format.version()
                        )));
                    }
                    validate_blob_string(
                        &tree.string_buffer,
                        node.type_str_offset,
                        &node.type_name,
                        "type",
                    )?;
                    validate_blob_string(
                        &tree.string_buffer,
                        node.name_str_offset,
                        &node.name,
                        "name",
                    )?;
                }
            }
            stack.try_reserve(node.children.len()).map_err(|error| {
                BinaryError::memory_error(format!(
                    "Failed to reserve TypeTree validation stack: {error}"
                ))
            })?;
            stack.extend(node.children.iter().rev());
        }
        Ok(())
    }

    /// Validate a single node and its children
    fn validate_node(node: &TypeTreeNode, expected_level: i32) -> Result<()> {
        if node.type_name.is_empty() {
            return Err(BinaryError::invalid_data("Node has empty type name"));
        }

        if node.level != expected_level {
            return Err(BinaryError::invalid_data(format!(
                "Node level mismatch: expected {}, got {}",
                expected_level, node.level
            )));
        }

        if node.byte_size < -1 {
            return Err(BinaryError::invalid_data("Invalid byte size"));
        }

        // Validate children
        for child in &node.children {
            Self::validate_node(child, expected_level + 1)?;
        }

        Ok(())
    }

    /// Get parsing statistics
    pub fn get_parsing_stats(tree: &TypeTree) -> ParsingStats {
        let mut stats = (0usize, 0i32, 0usize, 0usize); // (total_nodes, max_depth, primitive_count, array_count)

        fn count_nodes(node: &TypeTreeNode, depth: i32, stats: &mut (usize, i32, usize, usize)) {
            stats.0 += 1; // total_nodes
            stats.1 = stats.1.max(depth); // max_depth

            if node.is_primitive() {
                stats.2 += 1; // primitive_count
            }
            if node.is_array() {
                stats.3 += 1; // array_count
            }

            for child in &node.children {
                count_nodes(child, depth + 1, stats);
            }
        }

        for node in &tree.nodes {
            count_nodes(node, 0, &mut stats);
        }

        ParsingStats {
            total_nodes: stats.0,
            root_nodes: tree.nodes.len(),
            max_depth: stats.1,
            primitive_count: stats.2,
            array_count: stats.3,
            string_buffer_size: tree.string_buffer.len(),
            version: tree.version,
        }
    }
}

fn copy_string(value: &str) -> Result<String> {
    let mut owned = String::new();
    owned.try_reserve_exact(value.len()).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {} TypeTree string bytes: {error}",
            value.len()
        ))
    })?;
    owned.push_str(value);
    Ok(owned)
}

fn validate_legacy_only_fields(
    node: &TypeTreeNode,
    format: SerializedFileFormat,
    has_variable_count: bool,
) -> Result<()> {
    if (!has_variable_count && node.variable_count != 0)
        || node.type_str_offset != 0
        || node.name_str_offset != 0
        || node.ref_type_hash != 0
    {
        return Err(BinaryError::invalid_data(format!(
            "SerializedFile v{} legacy TypeTree contains fields absent from its wire encoding",
            format.version()
        )));
    }
    Ok(())
}

fn validate_blob_string(
    string_buffer: &[u8],
    offset: u32,
    expected: &str,
    label: &str,
) -> Result<()> {
    let resolved = TypeTreeParser::resolve_string(string_buffer, offset)?;
    if resolved != expected {
        return Err(BinaryError::invalid_data(format!(
            "TypeTree {label} offset {offset:#010x} resolves to {resolved:?}, expected {expected:?}"
        )));
    }
    Ok(())
}

fn read_non_negative_count(input: &mut (impl BinaryInput + ?Sized), label: &str) -> Result<u64> {
    let count = input.read_i32()?;
    u64::try_from(count)
        .map_err(|_| BinaryError::invalid_data(format!("Negative {label} count: {count}")))
}

fn ensure_count_fits_remaining(
    input: &(impl BinaryInput + ?Sized),
    count: u64,
    minimum_entry_size: usize,
    label: &str,
) -> Result<()> {
    let minimum_bytes = count
        .checked_mul(minimum_entry_size as u64)
        .ok_or_else(|| BinaryError::invalid_data(format!("{label} byte size overflow")))?;
    if minimum_bytes > input.remaining() {
        return Err(not_enough_data_u64(minimum_bytes, input.remaining()));
    }
    Ok(())
}

fn count_to_usize(count: u64, label: &str) -> Result<usize> {
    usize::try_from(count)
        .map_err(|_| BinaryError::memory_error(format!("{label} count does not fit in usize")))
}

const fn legacy_min_node_size(encoding: TypeTreeEncoding) -> usize {
    match encoding {
        TypeTreeEncoding::LegacyV2 => 30,
        TypeTreeEncoding::LegacyV3 => 18,
        TypeTreeEncoding::LegacyStandard => 26,
        TypeTreeEncoding::Blob | TypeTreeEncoding::BlobWithRefTypeHash => 0,
    }
}

fn read_cstring_limited(input: &mut (impl BinaryInput + ?Sized), _label: &str) -> Result<String> {
    input.read_cstring_limited(BinaryReader::DEFAULT_MAX_STRING_LEN)
}

fn close_hierarchy_node(
    stack: &mut Vec<TypeTreeNode>,
    roots: &mut Vec<TypeTreeNode>,
) -> Result<()> {
    let Some(node) = stack.pop() else {
        return Ok(());
    };
    if let Some(parent) = stack.last_mut() {
        parent.children.try_reserve(1).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve TypeTree child relationship: {error}"
            ))
        })?;
        parent.children.push(node);
    } else {
        roots.try_reserve(1).map_err(|error| {
            BinaryError::memory_error(format!("Failed to reserve TypeTree root: {error}"))
        })?;
        roots.push(node);
    }
    Ok(())
}

/// Parsing statistics
#[derive(Debug, Clone)]
pub struct ParsingStats {
    pub total_nodes: usize,
    pub root_nodes: usize,
    pub max_depth: i32,
    pub primitive_count: usize,
    pub array_count: usize,
    pub string_buffer_size: usize,
    pub version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::ByteOrder;

    #[test]
    fn test_parser_creation() {
        // Basic test to ensure parser methods exist
        let _dummy = 1 + 1;
        assert_eq!(_dummy, 2);
    }

    #[test]
    fn test_string_buffer_parsing() {
        let buffer = b"hello\0world\0test\0";
        let result = TypeTreeParser::get_string_from_buffer(buffer, 0).unwrap();
        assert_eq!(result, "hello");

        let result = TypeTreeParser::get_string_from_buffer(buffer, 6).unwrap();
        assert_eq!(result, "world");

        let result = TypeTreeParser::get_string_from_buffer(buffer, 12).unwrap();
        assert_eq!(result, "test");
    }

    #[test]
    fn test_common_string_flag_resolves_known_offsets() {
        const COMMON_STRING_FLAG: u32 = 0x8000_0000;

        let local = b"ignored\0";

        // offset 0 in the common string buffer maps to "AABB"
        let result = TypeTreeParser::resolve_string(local, COMMON_STRING_FLAG).unwrap();
        assert_eq!(result, "AABB");
    }

    #[test]
    fn test_common_string_flag_rejects_unknown_offsets() {
        const COMMON_STRING_FLAG: u32 = 0x8000_0000;

        let error = TypeTreeParser::resolve_string(b"ignored\0", COMMON_STRING_FLAG | 123_456)
            .expect_err("unknown common-string offsets must not silently lose metadata");
        assert!(
            error
                .to_string()
                .contains("Unknown TypeTree common-string offset 123456")
        );
    }

    #[test]
    fn test_blob_typetree_parsing_resolves_common_strings() {
        const COMMON_STRING_FLAG: u32 = 0x8000_0000;

        let mut data = Vec::new();
        data.extend_from_slice(&(1i32).to_le_bytes()); // node_count
        data.extend_from_slice(&(0i32).to_le_bytes()); // string_buffer_size

        // TypeTreeNode (blob)
        data.extend_from_slice(&(1u16).to_le_bytes()); // version
        data.push(0u8); // level
        data.push(0u8); // type_flags
        data.extend_from_slice(&COMMON_STRING_FLAG.to_le_bytes()); // type_str_offset => "AABB"
        data.extend_from_slice(&COMMON_STRING_FLAG.to_le_bytes()); // name_str_offset => "AABB"
        data.extend_from_slice(&(0i32).to_le_bytes()); // byte_size
        data.extend_from_slice(&(0i32).to_le_bytes()); // index
        data.extend_from_slice(&(0i32).to_le_bytes()); // meta_flags
        data.extend_from_slice(&(0u64).to_le_bytes()); // ref_type_hash (version >= 19)

        let mut budget = AssetLoadBudget::default();
        let tree =
            TypeTreeParser::from_blob_bytes(&data, ByteOrder::Little, 19, &mut budget).unwrap();

        assert_eq!(tree.nodes.len(), 1);
        assert_eq!(tree.nodes[0].type_name, "AABB");
        assert_eq!(tree.nodes[0].name, "AABB");
    }

    #[test]
    fn repeated_blob_strings_charge_every_owned_copy_before_allocation() {
        let value = b"duplicate\0";
        let mut data = Vec::new();
        data.extend_from_slice(&2_i32.to_le_bytes());
        data.extend_from_slice(&i32::try_from(value.len()).unwrap().to_le_bytes());
        for _ in 0..2 {
            data.extend_from_slice(&1_i16.to_le_bytes());
            data.push(0);
            data.push(0);
            data.extend_from_slice(&0_u32.to_le_bytes());
            data.extend_from_slice(&0_u32.to_le_bytes());
            data.extend_from_slice(&0_i32.to_le_bytes());
            data.extend_from_slice(&0_i32.to_le_bytes());
            data.extend_from_slice(&0_i32.to_le_bytes());
        }
        data.extend_from_slice(value);

        let owned_string_bytes = 2_u64 * 2 * u64::try_from(value.len() - 1).unwrap();
        let exact_bytes = u64::try_from(data.len()).unwrap() + owned_string_bytes;
        let mut exact = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes: exact_bytes,
            ..Default::default()
        })
        .unwrap();
        let tree = TypeTreeParser::from_blob_bytes(&data, ByteOrder::Little, 17, &mut exact)
            .expect("every owned string copy fits the exact allocation budget");
        assert_eq!(tree.nodes.len(), 2);
        assert_eq!(exact.usage().bytes, exact_bytes);

        let mut short = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes: exact_bytes - 1,
            ..Default::default()
        })
        .unwrap();
        let error = TypeTreeParser::from_blob_bytes(&data, ByteOrder::Little, 17, &mut short)
            .expect_err("one byte short must fail before cloning repeated strings");
        assert!(matches!(
            error,
            BinaryError::Budget(unity_asset_core::BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == exact_bytes - 1 && requested == exact_bytes
        ));
        assert_eq!(short.usage().bytes, u64::try_from(data.len()).unwrap());
    }
}
