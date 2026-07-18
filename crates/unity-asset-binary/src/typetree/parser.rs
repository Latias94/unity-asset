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
use std::mem::size_of;
use unity_asset_core::AssetLoadBudget;

pub const MAX_TYPE_TREE_NODES: usize = 1_000_000;
pub const MAX_TYPE_TREE_DEPTH: usize = 512;
pub const MAX_TYPE_TREE_STRING_BUFFER: usize = BinaryReader::DEFAULT_MAX_STRING_LEN;
const COMMON_STRING_FLAG: u32 = 0x8000_0000;

struct BlobTreePlan {
    node_count: usize,
    node_table_start: u64,
    node_width: u64,
    payload_end: u64,
    root_count: usize,
    max_stack: usize,
    member_count: u64,
    backing_bytes: u64,
    future_bytes: u64,
}

struct BlobTreeLayout {
    node_count: usize,
    string_buffer_size: usize,
    node_table_start: u64,
    node_table_end: u64,
    node_width: u64,
    payload_end: u64,
    root_count: usize,
    max_stack: usize,
    member_count: usize,
    owned_string_bytes: u64,
}

impl BlobTreePlan {
    fn new(layout: BlobTreeLayout) -> Result<Self> {
        let BlobTreeLayout {
            node_count,
            string_buffer_size,
            node_table_start,
            node_table_end,
            node_width,
            payload_end,
            root_count,
            max_stack,
            member_count,
            owned_string_bytes,
        } = layout;
        let child_count = node_count
            .checked_sub(root_count)
            .ok_or_else(|| BinaryError::invalid_data("TypeTree child count underflow"))?;
        let backing_bytes = [
            backing_bytes::<TypeTreeNode>(node_count, "TypeTree flat node backing")?,
            backing_bytes::<TypeTreeNode>(root_count, "TypeTree root backing")?,
            backing_bytes::<TypeTreeNode>(max_stack, "TypeTree hierarchy stack backing")?,
            backing_bytes::<TypeTreeNode>(child_count, "TypeTree child backing")?,
            backing_bytes::<usize>(node_count, "TypeTree child-count plan backing")?,
        ]
        .into_iter()
        .try_fold(0_u64, |total, value| {
            total.checked_add(value).ok_or_else(|| {
                BinaryError::memory_error("TypeTree retained backing total overflow")
            })
        })?;
        let node_bytes = node_table_end
            .checked_sub(node_table_start)
            .ok_or_else(|| BinaryError::invalid_data("TypeTree node table range underflow"))?;
        let future_bytes = [
            backing_bytes,
            node_bytes,
            usize_to_u64(node_count, "TypeTree hierarchy plan scan")?,
            usize_to_u64(string_buffer_size, "TypeTree string buffer size")?,
            owned_string_bytes,
        ]
        .into_iter()
        .try_fold(0_u64, |total, value| {
            total
                .checked_add(value)
                .ok_or_else(|| BinaryError::memory_error("TypeTree prepared byte total overflow"))
        })?;

        Ok(Self {
            node_count,
            node_table_start,
            node_width,
            payload_end,
            root_count,
            max_stack,
            member_count: usize_to_u64(member_count, "TypeTree member count")?,
            backing_bytes,
            future_bytes,
        })
    }

    fn commit_budget(&self, input: &mut (impl BinaryInput + ?Sized)) -> Result<()> {
        let node_count = usize_to_u64(self.node_count, "TypeTree node count")?;
        input.check_entries(node_count)?;
        input.check_members(self.member_count)?;
        input.check_bytes(self.future_bytes)?;
        if self.max_stack > 0 {
            let max_depth = u32::try_from(self.max_stack - 1).map_err(|_| {
                BinaryError::invalid_data("TypeTree maximum depth does not fit in u32")
            })?;
            input.check_depth(max_depth)?;
        }

        input.consume_entries(node_count)?;
        input.consume_members(self.member_count)?;
        input.consume_bytes(self.backing_bytes)?;
        if self.max_stack > 0 {
            let max_depth = u32::try_from(self.max_stack - 1).map_err(|_| {
                BinaryError::invalid_data("TypeTree maximum depth does not fit in u32")
            })?;
            input.observe_depth(max_depth)?;
        }
        Ok(())
    }
}

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
        let root_backing = backing_bytes::<TypeTreeNode>(1, "legacy TypeTree root")?;
        input.check_bytes(root_backing)?;
        input.consume_bytes(root_backing)?;
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
        let remaining_nodes = (MAX_TYPE_TREE_NODES as u64)
            .checked_sub(*nodes_read)
            .ok_or_else(|| {
                BinaryError::ResourceLimitExceeded("TypeTree node count exceeds limit".into())
            })?;
        if child_count > remaining_nodes {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "TypeTree child count {child_count} exceeds remaining node limit {remaining_nodes}"
            )));
        }
        let child_count = count_to_usize(child_count, "legacy TypeTree child")?;
        let child_depth = if child_count == 0 {
            None
        } else {
            Some(
                depth
                    .checked_add(1)
                    .ok_or_else(|| BinaryError::invalid_data("TypeTree depth overflows u32"))?,
            )
        };
        let child_backing =
            backing_bytes::<TypeTreeNode>(child_count, "legacy TypeTree child backing")?;
        let child_count_u64 = u64::try_from(child_count).map_err(|_| {
            BinaryError::memory_error("legacy TypeTree child count does not fit in u64")
        })?;
        input.check_entries(child_count_u64)?;
        input.check_members(child_count_u64)?;
        input.check_bytes(child_backing)?;
        if let Some(child_depth) = child_depth {
            if u64::from(child_depth) > MAX_TYPE_TREE_DEPTH as u64 {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "TypeTree depth {child_depth} exceeds limit {MAX_TYPE_TREE_DEPTH}"
                )));
            }
            input.check_depth(child_depth)?;
        }
        input.consume_members(child_count_u64)?;
        input.consume_bytes(child_backing)?;
        if let Some(child_depth) = child_depth {
            input.observe_depth(child_depth)?;
        }
        node.children
            .try_reserve_exact(child_count)
            .map_err(|error| {
                BinaryError::memory_error(format!(
                    "Failed to reserve {child_count} legacy TypeTree children: {error}"
                ))
            })?;
        for _ in 0..child_count {
            let child_depth = child_depth.ok_or_else(|| {
                BinaryError::invalid_data("positive child count has no child depth")
            })?;
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
        let node_count = count_to_usize(node_count, "TypeTree node")?;
        let string_buffer_size = count_to_usize(string_buffer_size, "TypeTree string buffer")?;
        let plan = Self::preflight_blob_tree(input, node_count, string_buffer_size, node_width)?;
        plan.commit_budget(input)?;

        let child_counts = Self::prepare_blob_child_counts(input, &plan)?;
        input.set_position(plan.node_table_start)?;
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
        Self::reserve_blob_hierarchy(&mut tree.nodes, child_counts)?;
        Self::build_hierarchy(&mut tree, &plan)?;
        debug_assert_eq!(input.position(), plan.payload_end);
        Ok(tree)
    }

    fn preflight_blob_tree(
        input: &mut (impl BinaryInput + ?Sized),
        node_count: usize,
        string_buffer_size: usize,
        node_width: u64,
    ) -> Result<BlobTreePlan> {
        let node_table_start = input.position();
        let node_count_u64 = usize_to_u64(node_count, "TypeTree node count")?;
        let node_bytes = node_count_u64
            .checked_mul(node_width)
            .ok_or_else(|| BinaryError::invalid_data("TypeTree node table size overflow"))?;
        let node_table_end = node_table_start
            .checked_add(node_bytes)
            .ok_or_else(|| BinaryError::invalid_data("TypeTree node table range overflow"))?;
        let string_buffer_size_u64 =
            usize_to_u64(string_buffer_size, "TypeTree string buffer size")?;
        let payload_end = node_table_end
            .checked_add(string_buffer_size_u64)
            .ok_or_else(|| BinaryError::invalid_data("TypeTree payload range overflow"))?;

        let mut previous_level: Option<usize> = None;
        let mut root_count = 0_usize;
        let mut max_stack = 0_usize;
        let mut owned_string_bytes = 0_u64;

        for node_index in 0..node_count {
            let node_start = input.position();
            let node_end = node_start
                .checked_add(node_width)
                .ok_or_else(|| BinaryError::invalid_data("TypeTree node range overflow"))?;
            if node_end > node_table_end {
                return Err(BinaryError::invalid_data(
                    "TypeTree preflight left the declared node table",
                ));
            }

            let _version = input.read_i16()?;
            let level = input.read_u8()?;
            let _type_flags = input.read_u8()?;
            let type_str_offset = input.read_u32()?;
            let name_str_offset = input.read_u32()?;
            input.set_position(node_end)?;

            let level = usize::from(level);
            if level > MAX_TYPE_TREE_DEPTH {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "TypeTree node level {level} exceeds limit {MAX_TYPE_TREE_DEPTH}"
                )));
            }
            let level_u32 = u32::try_from(level)
                .map_err(|_| BinaryError::invalid_data("TypeTree level does not fit in u32"))?;
            input.check_depth(level_u32)?;

            match previous_level {
                None if level != 0 => {
                    return Err(BinaryError::invalid_data(format!(
                        "First TypeTree node has non-root level {level}"
                    )));
                }
                Some(previous) if level > previous.saturating_add(1) => {
                    return Err(BinaryError::invalid_data(format!(
                        "TypeTree node level jumps from {previous} to {level} at node {node_index}"
                    )));
                }
                _ => {}
            }
            previous_level = Some(level);
            if level == 0 {
                root_count = root_count
                    .checked_add(1)
                    .ok_or_else(|| BinaryError::invalid_data("TypeTree root count overflow"))?;
            }
            max_stack = max_stack.max(level.checked_add(1).ok_or_else(|| {
                BinaryError::invalid_data("TypeTree hierarchy stack depth overflow")
            })?);

            for offset in [type_str_offset, name_str_offset] {
                let string_bytes =
                    preflight_blob_string(input, node_table_end, string_buffer_size_u64, offset)?;
                owned_string_bytes =
                    owned_string_bytes
                        .checked_add(string_bytes)
                        .ok_or_else(|| {
                            BinaryError::memory_error("TypeTree owned string total overflow")
                        })?;
            }
        }
        input.set_position(node_table_end)?;

        let member_count = node_count
            .checked_sub(root_count)
            .ok_or_else(|| BinaryError::invalid_data("TypeTree member count underflow"))?;
        BlobTreePlan::new(BlobTreeLayout {
            node_count,
            string_buffer_size,
            node_table_start,
            node_table_end,
            node_width,
            payload_end,
            root_count,
            max_stack,
            member_count,
            owned_string_bytes,
        })
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

    fn prepare_blob_child_counts(
        input: &mut (impl BinaryInput + ?Sized),
        plan: &BlobTreePlan,
    ) -> Result<Vec<usize>> {
        let mut child_counts = Vec::new();
        child_counts
            .try_reserve_exact(plan.node_count)
            .map_err(|error| {
                BinaryError::memory_error(format!(
                    "Failed to reserve TypeTree child-count plan: {error}"
                ))
            })?;
        child_counts.resize(plan.node_count, 0_usize);

        let mut parent_at_level = [0_usize; MAX_TYPE_TREE_DEPTH + 1];
        let mut node_start = plan.node_table_start;
        for index in 0..plan.node_count {
            let level_position = node_start
                .checked_add(2)
                .ok_or_else(|| BinaryError::invalid_data("TypeTree level position overflow"))?;
            let node_end = node_start
                .checked_add(plan.node_width)
                .ok_or_else(|| BinaryError::invalid_data("TypeTree node range overflow"))?;
            input.set_position(level_position)?;
            let level = usize::from(input.read_u8()?);
            input.set_position(node_end)?;
            if level > MAX_TYPE_TREE_DEPTH {
                return Err(BinaryError::invalid_data(
                    "TypeTree level changed after successful preflight",
                ));
            }
            if level > 0 {
                let parent = parent_at_level[level - 1];
                child_counts[parent] = child_counts[parent].checked_add(1).ok_or_else(|| {
                    BinaryError::invalid_data("TypeTree direct child count overflow")
                })?;
            }
            parent_at_level[level] = index;
            node_start = node_end;
        }
        input.set_position(plan.node_table_start)?;
        Ok(child_counts)
    }

    fn reserve_blob_hierarchy(nodes: &mut [TypeTreeNode], child_counts: Vec<usize>) -> Result<()> {
        if nodes.len() != child_counts.len() {
            return Err(BinaryError::invalid_data(
                "TypeTree child-count plan length changed during apply",
            ));
        }
        for (node, child_count) in nodes.iter_mut().zip(child_counts) {
            node.children
                .try_reserve_exact(child_count)
                .map_err(|error| {
                    BinaryError::memory_error(format!(
                        "Failed to reserve {child_count} TypeTree children: {error}"
                    ))
                })?;
        }
        Ok(())
    }

    /// Builds the hierarchy using only capacities committed by the blob preflight.
    fn build_hierarchy(tree: &mut TypeTree, plan: &BlobTreePlan) -> Result<()> {
        if tree.nodes.is_empty() {
            return Ok(());
        }

        let flat_nodes = std::mem::take(&mut tree.nodes);
        let mut stack: Vec<TypeTreeNode> = Vec::new();
        stack.try_reserve_exact(plan.max_stack).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve TypeTree hierarchy stack: {error}"
            ))
        })?;
        let mut roots = Vec::new();
        roots.try_reserve_exact(plan.root_count).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {} TypeTree roots: {error}",
                plan.root_count
            ))
        })?;

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
                close_prepared_hierarchy_node(&mut stack, &mut roots)?;
            }
            if stack.len() != level {
                return Err(BinaryError::invalid_data(format!(
                    "TypeTree node level jumps from {} to {level}",
                    stack.len().saturating_sub(1)
                )));
            }
            node.children.clear();
            if stack.len() == stack.capacity() {
                return Err(BinaryError::invalid_data(
                    "TypeTree hierarchy stack exceeded its preflight capacity",
                ));
            }
            stack.push(node);
        }
        while !stack.is_empty() {
            close_prepared_hierarchy_node(&mut stack, &mut roots)?;
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
            Self::validate_node(node, 0, 0).map_err(|error| {
                if error.is_resource_error() {
                    error
                } else {
                    BinaryError::generic(format!("Node {i} validation failed: {error}"))
                }
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

        for node in &tree.nodes {
            Self::validate_node_for_format(node, tree, format, encoding)?;
        }
        Ok(())
    }

    fn validate_node_for_format(
        node: &TypeTreeNode,
        tree: &TypeTree,
        format: SerializedFileFormat,
        encoding: TypeTreeEncoding,
    ) -> Result<()> {
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
        for child in &node.children {
            Self::validate_node_for_format(child, tree, format, encoding)?;
        }
        Ok(())
    }

    /// Validate a single node and its children
    fn validate_node(node: &TypeTreeNode, expected_level: i32, depth: usize) -> Result<()> {
        if depth > MAX_TYPE_TREE_DEPTH {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "TypeTree node depth {depth} exceeds limit {MAX_TYPE_TREE_DEPTH}"
            )));
        }
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
            let child_level = expected_level
                .checked_add(1)
                .ok_or_else(|| BinaryError::invalid_data("TypeTree level overflows i32"))?;
            let child_depth = depth
                .checked_add(1)
                .ok_or_else(|| BinaryError::invalid_data("TypeTree depth overflows usize"))?;
            Self::validate_node(child, child_level, child_depth)?;
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

fn preflight_blob_string(
    input: &mut (impl BinaryInput + ?Sized),
    string_buffer_start: u64,
    string_buffer_size: u64,
    offset: u32,
) -> Result<u64> {
    if (offset & COMMON_STRING_FLAG) != 0 {
        let common_offset = offset & !COMMON_STRING_FLAG;
        let value = common_strings::get_common_string(common_offset).ok_or_else(|| {
            BinaryError::invalid_data(format!(
                "Unknown TypeTree common-string offset {common_offset}"
            ))
        })?;
        return usize_to_u64(value.len(), "TypeTree common string length");
    }

    let local_offset = u64::from(offset);
    if local_offset >= string_buffer_size {
        return Err(BinaryError::invalid_data(format!(
            "TypeTree local string offset {offset} is outside buffer length {string_buffer_size}"
        )));
    }

    let return_position = input.position();
    let result = (|| {
        let string_start = string_buffer_start
            .checked_add(local_offset)
            .ok_or_else(|| BinaryError::invalid_data("TypeTree string start overflow"))?;
        let string_buffer_end = string_buffer_start
            .checked_add(string_buffer_size)
            .ok_or_else(|| BinaryError::invalid_data("TypeTree string buffer range overflow"))?;

        if local_offset > 0 {
            input.set_position(string_start - 1)?;
            if input.read_u8()? != 0 {
                return Err(BinaryError::invalid_data(format!(
                    "TypeTree local string offset {offset} does not point to a string start"
                )));
            }
        }
        input.set_position(string_start)?;

        let mut utf8 = Utf8Preflight::default();
        let mut length = 0_u64;
        while input.position() < string_buffer_end {
            let byte = input.read_u8()?;
            if byte == 0 {
                if !utf8.is_complete() {
                    return Err(BinaryError::invalid_data(format!(
                        "Invalid UTF-8 at TypeTree string offset {offset}"
                    )));
                }
                return Ok(length);
            }
            if !utf8.accept(byte) {
                return Err(BinaryError::invalid_data(format!(
                    "Invalid UTF-8 at TypeTree string offset {offset}"
                )));
            }
            length = length.checked_add(1).ok_or_else(|| {
                BinaryError::memory_error("TypeTree local string length overflow")
            })?;
        }
        Err(BinaryError::invalid_data(format!(
            "TypeTree string at local offset {offset} has no null terminator"
        )))
    })();
    input.set_position(return_position)?;
    result
}

#[derive(Default)]
struct Utf8Preflight {
    remaining: u8,
    next_min: u8,
    next_max: u8,
}

impl Utf8Preflight {
    fn accept(&mut self, byte: u8) -> bool {
        if self.remaining > 0 {
            if byte < self.next_min || byte > self.next_max {
                return false;
            }
            self.remaining -= 1;
            self.next_min = 0x80;
            self.next_max = 0xbf;
            return true;
        }

        match byte {
            0x01..=0x7f => true,
            0xc2..=0xdf => self.begin_sequence(1, 0x80, 0xbf),
            0xe0 => self.begin_sequence(2, 0xa0, 0xbf),
            0xe1..=0xec | 0xee..=0xef => self.begin_sequence(2, 0x80, 0xbf),
            0xed => self.begin_sequence(2, 0x80, 0x9f),
            0xf0 => self.begin_sequence(3, 0x90, 0xbf),
            0xf1..=0xf3 => self.begin_sequence(3, 0x80, 0xbf),
            0xf4 => self.begin_sequence(3, 0x80, 0x8f),
            _ => false,
        }
    }

    fn begin_sequence(&mut self, remaining: u8, next_min: u8, next_max: u8) -> bool {
        self.remaining = remaining;
        self.next_min = next_min;
        self.next_max = next_max;
        true
    }

    const fn is_complete(&self) -> bool {
        self.remaining == 0
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

fn usize_to_u64(value: usize, label: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| BinaryError::memory_error(format!("{label} does not fit in u64")))
}

fn backing_bytes<T>(count: usize, label: &str) -> Result<u64> {
    let bytes = size_of::<T>()
        .checked_mul(count)
        .ok_or_else(|| BinaryError::memory_error(format!("{label} size overflow")))?;
    usize_to_u64(bytes, label)
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

fn close_prepared_hierarchy_node(
    stack: &mut Vec<TypeTreeNode>,
    roots: &mut Vec<TypeTreeNode>,
) -> Result<()> {
    let Some(node) = stack.pop() else {
        return Ok(());
    };
    if let Some(parent) = stack.last_mut() {
        if parent.children.len() == parent.children.capacity() {
            return Err(BinaryError::invalid_data(
                "TypeTree child count exceeded its preflight capacity",
            ));
        }
        parent.children.push(node);
    } else {
        if roots.len() == roots.capacity() {
            return Err(BinaryError::invalid_data(
                "TypeTree root count exceeded its preflight capacity",
            ));
        }
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
    use crate::asset::format::SerializedFileFormat;
    use crate::random_access::{BorrowedBytes, ByteCursor};
    use crate::reader::ByteOrder;
    use unity_asset_core::{AssetLoadLimits, BudgetError};

    fn blob_tree(levels: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&i32::try_from(levels.len()).unwrap().to_le_bytes());
        data.extend_from_slice(&0_i32.to_le_bytes());
        for (index, level) in levels.iter().copied().enumerate() {
            data.extend_from_slice(&1_i16.to_le_bytes());
            data.push(level);
            data.push(0);
            data.extend_from_slice(&COMMON_STRING_FLAG.to_le_bytes());
            data.extend_from_slice(&COMMON_STRING_FLAG.to_le_bytes());
            data.extend_from_slice(&0_i32.to_le_bytes());
            data.extend_from_slice(&i32::try_from(index).unwrap().to_le_bytes());
            data.extend_from_slice(&0_i32.to_le_bytes());
        }
        data
    }

    fn legacy_standard_node(type_name: &str, name: &str, child_count: i32) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(type_name.as_bytes());
        data.push(0);
        data.extend_from_slice(name.as_bytes());
        data.push(0);
        data.extend_from_slice(&0_i32.to_le_bytes());
        data.extend_from_slice(&0_i32.to_le_bytes());
        data.extend_from_slice(&0_i32.to_le_bytes());
        data.extend_from_slice(&1_i32.to_le_bytes());
        data.extend_from_slice(&0_i32.to_le_bytes());
        data.extend_from_slice(&child_count.to_le_bytes());
        data
    }

    fn legacy_tree_with_depth(max_depth: usize) -> TypeTree {
        let mut node = TypeTreeNode::with_info("Node".into(), "field".into(), 0);
        node.level = i32::try_from(max_depth).unwrap();
        for level in (0..max_depth).rev() {
            let mut parent = TypeTreeNode::with_info("Node".into(), "field".into(), 0);
            parent.level = i32::try_from(level).unwrap();
            parent.children.push(node);
            node = parent;
        }
        let mut tree = TypeTree::new();
        tree.version = 11;
        tree.nodes.push(node);
        tree
    }

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
    fn repeated_blob_strings_are_included_in_atomic_blob_preflight() {
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

        let mut probe = AssetLoadBudget::default();
        let tree = TypeTreeParser::from_blob_bytes(&data, ByteOrder::Little, 17, &mut probe)
            .expect("probe budget accepts repeated strings");
        assert_eq!(tree.nodes.len(), 2);
        let exact_bytes = probe.usage().bytes;
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
        assert!(short.usage().bytes < exact_bytes);
        assert_eq!(short.usage().entries, 0);
        assert_eq!(short.usage().members, 0);
        assert_eq!(short.usage().max_observed_depth, 0);
    }

    #[test]
    fn blob_preflight_one_short_is_atomic_and_restores_apply_position() {
        let data = blob_tree(&[0, 1, 2, 1, 0, 1]);
        let mut probe = AssetLoadBudget::default();
        TypeTreeParser::from_blob_bytes(&data, ByteOrder::Little, 17, &mut probe).unwrap();
        let exact_bytes = probe.usage().bytes;

        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_bytes - 1,
            ..Default::default()
        })
        .unwrap();
        let source = BorrowedBytes::new(&data);
        let (error, position) = {
            let mut input = ByteCursor::new(&source, ByteOrder::Little, &mut budget).unwrap();
            let error = TypeTreeParser::from_input_with_format(
                &mut input,
                SerializedFileFormat::new(17).unwrap(),
            )
            .unwrap_err();
            (error, input.position())
        };
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == exact_bytes - 1 && requested == exact_bytes
        ));
        assert_eq!(position, 8 + 24 * u64::try_from(6_usize).unwrap());
        assert_eq!(budget.usage().bytes, 8 + 12 * 6);
        assert_eq!(budget.usage().entries, 0);
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().max_observed_depth, 0);

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_bytes,
            ..Default::default()
        })
        .unwrap();
        let tree = TypeTreeParser::from_blob_bytes(&data, ByteOrder::Little, 17, &mut exact)
            .expect("exact prepared budget succeeds");
        assert_eq!(exact.usage().bytes, exact_bytes);
        assert_eq!(tree.nodes.len(), 2);
        assert_eq!(tree.nodes[0].children.len(), 2);
        assert_eq!(tree.nodes[0].children[0].children.len(), 1);
        assert_eq!(tree.nodes[1].children.len(), 1);
    }

    #[test]
    fn blob_depth_failure_precedes_all_backing_charges() {
        let mut levels = vec![0_u8; 10_000];
        levels[1] = 1;
        levels[2] = 2;
        let data = blob_tree(&levels);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: 1,
            ..Default::default()
        })
        .unwrap();
        let source = BorrowedBytes::new(&data);
        let (error, position) = {
            let mut input = ByteCursor::new(&source, ByteOrder::Little, &mut budget).unwrap();
            let error = TypeTreeParser::from_input_with_format(
                &mut input,
                SerializedFileFormat::new(17).unwrap(),
            )
            .unwrap_err();
            (error, input.position())
        };
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "depth",
                limit: 1,
                requested: 2,
            })
        ));
        assert_eq!(position, 8 + 24 * 3);
        assert_eq!(budget.usage().bytes, 8 + 12 * 3);
        assert_eq!(budget.usage().entries, 0);
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().max_observed_depth, 0);
    }

    #[test]
    fn blob_depth_preflight_composes_with_container_scope() {
        let data = blob_tree(&[0, 1]);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: 2,
            ..Default::default()
        })
        .unwrap();
        let error = {
            let mut scoped = budget.enter_depth(2).unwrap();
            TypeTreeParser::from_blob_bytes(&data, ByteOrder::Little, 17, &mut scoped).unwrap_err()
        };
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "depth",
                limit: 2,
                requested: 3,
            })
        ));
        assert_eq!(budget.usage().entries, 0);
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().max_observed_depth, 0);
    }

    #[test]
    fn blob_level_jump_is_rejected_before_backing_commit() {
        let data = blob_tree(&[0, 2]);
        let mut budget = AssetLoadBudget::default();
        let error = TypeTreeParser::from_blob_bytes(&data, ByteOrder::Little, 17, &mut budget)
            .expect_err("a blob node cannot skip its parent level");
        assert!(matches!(error, BinaryError::InvalidData(message) if message.contains("jumps")));
        assert_eq!(budget.usage().entries, 0);
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().max_observed_depth, 0);
    }

    #[test]
    fn blob_local_string_offset_is_rejected_before_backing_commit() {
        let mut data = Vec::new();
        data.extend_from_slice(&1_i32.to_le_bytes());
        data.extend_from_slice(&1_i32.to_le_bytes());
        data.extend_from_slice(&1_i16.to_le_bytes());
        data.push(0);
        data.push(0);
        data.extend_from_slice(&1_u32.to_le_bytes());
        data.extend_from_slice(&0_u32.to_le_bytes());
        data.extend_from_slice(&0_i32.to_le_bytes());
        data.extend_from_slice(&0_i32.to_le_bytes());
        data.extend_from_slice(&0_i32.to_le_bytes());
        data.push(0);

        let mut budget = AssetLoadBudget::default();
        let error = TypeTreeParser::from_blob_bytes(&data, ByteOrder::Little, 17, &mut budget)
            .expect_err("a local string offset must be inside the declared buffer");
        assert!(
            matches!(error, BinaryError::InvalidData(message) if message.contains("outside buffer"))
        );
        assert_eq!(budget.usage().entries, 0);
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().max_observed_depth, 0);
    }

    #[test]
    fn legacy_child_backing_is_checked_before_reserve() {
        let mut data = legacy_standard_node("Root", "root", 1);
        data.extend_from_slice(&legacy_standard_node("int", "value", 0));
        let root_wire_bytes = u64::try_from(legacy_standard_node("Root", "root", 1).len())
            .expect("test fixture length fits u64");
        let requested =
            root_wire_bytes + u64::try_from(size_of::<TypeTreeNode>()).expect("node size fits u64");
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: requested - 1,
            ..Default::default()
        })
        .unwrap();
        let error = TypeTreeParser::from_bytes(&data, ByteOrder::Little, 11, &mut budget)
            .expect_err("legacy child backing must be checked before reserve");
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested: actual,
            }) if limit == requested - 1 && actual == requested
        ));
        assert_eq!(budget.usage().bytes, root_wire_bytes);
        assert_eq!(budget.usage().entries, 1);
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().max_observed_depth, 0);
    }

    #[test]
    fn format_validation_is_allocation_free_at_the_depth_boundary() {
        let format = SerializedFileFormat::new(11).unwrap();
        TypeTreeParser::validate_for_format(&legacy_tree_with_depth(MAX_TYPE_TREE_DEPTH), format)
            .expect("the supported depth boundary validates without a heap traversal stack");

        let error = TypeTreeParser::validate_for_format(
            &legacy_tree_with_depth(MAX_TYPE_TREE_DEPTH + 1),
            format,
        )
        .expect_err("one level beyond the parser limit must be rejected");
        assert!(matches!(
            error,
            BinaryError::ResourceLimitExceeded(message)
                if message.contains("depth 513 exceeds limit 512")
        ));
    }
}
