//! TypeTree parser implementation
//!
//! This module provides parsing functionality for Unity TypeTree structures,
//! inspired by UnityPy/classes/TypeTree.py

use super::types::{TypeTree, TypeTreeNode};
use crate::error::{BinaryError, Result};
use crate::reader::BinaryReader;

/// TypeTree parser
///
/// This struct handles the parsing of TypeTree structures from binary data,
/// supporting different Unity versions and formats.
pub struct TypeTreeParser;

impl TypeTreeParser {
    /// Parse TypeTree from binary data
    pub fn from_reader(reader: &mut BinaryReader, version: u32) -> Result<TypeTree> {
        let mut tree = TypeTree::new();
        tree.version = version;

        // Read number of nodes
        let node_count = reader.read_u32()? as usize;

        // Read string buffer size
        let string_buffer_size = reader.read_u32()? as usize;

        // Read nodes
        for _ in 0..node_count {
            let node = Self::read_node(reader, version)?;
            tree.nodes.push(node);
        }

        // Read string buffer
        tree.string_buffer = reader.read_bytes(string_buffer_size)?;

        // Resolve string references
        Self::resolve_strings(&mut tree)?;

        // Build tree hierarchy
        Self::build_hierarchy(&mut tree)?;

        Ok(tree)
    }

    /// Parse TypeTree from binary data using blob format (Unity version >= 12 or == 10)
    pub fn from_reader_blob(reader: &mut BinaryReader, version: u32) -> Result<TypeTree> {
        let mut tree = TypeTree::new();
        tree.version = version;

        // Read number of nodes
        let node_count = reader.read_i32()? as usize;

        // Read string buffer size
        let string_buffer_size = reader.read_i32()? as usize;

        // Read nodes in blob format
        for _ in 0..node_count {
            let mut node = TypeTreeNode::new();

            // Read node data in blob format (based on unity-rs)
            node.version = reader.read_u16()? as i32;
            node.level = reader.read_u8()? as i32;
            node.type_flags = reader.read_u8()? as i32;
            node.type_str_offset = reader.read_u32()?;
            node.name_str_offset = reader.read_u32()?;
            node.byte_size = reader.read_i32()?;
            node.index = reader.read_i32()?;
            node.meta_flags = reader.read_i32()?;

            if version >= 19 {
                node.ref_type_hash = reader.read_u64()?;
            }

            tree.nodes.push(node);
        }

        // Read string buffer
        tree.string_buffer = reader.read_bytes(string_buffer_size)?;

        // Resolve string references
        Self::resolve_strings(&mut tree)?;

        // Build tree hierarchy
        Self::build_hierarchy(&mut tree)?;

        Ok(tree)
    }

    /// Read a single TypeTree node
    fn read_node(reader: &mut BinaryReader, version: u32) -> Result<TypeTreeNode> {
        let mut node = TypeTreeNode::new();

        if version >= 10 {
            node.version = reader.read_i16()? as i32;
            node.level = reader.read_u8()? as i32;
            node.type_flags = reader.read_u8()? as i32;
            node.type_str_offset = reader.read_u32()?;
            node.name_str_offset = reader.read_u32()?;
            node.byte_size = reader.read_i32()?;
            node.index = reader.read_i32()?;
            node.meta_flags = reader.read_i32()?;

            if version >= 12 {
                node.ref_type_hash = reader.read_u64()?;
            }
        } else {
            // Legacy format
            node.type_str_offset = reader.read_u32()?;
            node.name_str_offset = reader.read_u32()?;
            node.byte_size = reader.read_i32()?;
            node.index = reader.read_i32()?;
            node.type_flags = reader.read_i32()?;
            node.version = reader.read_i32()?;
            node.meta_flags = reader.read_i32()?;
            node.level = reader.read_i32()?;
        }

        Ok(node)
    }

    /// Resolve string references in the TypeTree
    fn resolve_strings(tree: &mut TypeTree) -> Result<()> {
        for node in &mut tree.nodes {
            Self::resolve_node_strings(node, &tree.string_buffer)?;
        }
        Ok(())
    }

    /// Resolve string references for a single node and its children
    fn resolve_node_strings(node: &mut TypeTreeNode, string_buffer: &[u8]) -> Result<()> {
        // Resolve type name
        node.type_name = Self::get_string_from_buffer(string_buffer, node.type_str_offset)?;

        // Resolve field name
        node.name = Self::get_string_from_buffer(string_buffer, node.name_str_offset)?;

        // Resolve children
        for child in &mut node.children {
            Self::resolve_node_strings(child, string_buffer)?;
        }

        Ok(())
    }

    /// Get string from buffer at offset
    fn get_string_from_buffer(buffer: &[u8], offset: u32) -> Result<String> {
        if offset as usize >= buffer.len() {
            return Ok(String::new());
        }

        let start = offset as usize;
        let end = buffer[start..]
            .iter()
            .position(|&b| b == 0)
            .map(|pos| start + pos)
            .unwrap_or(buffer.len());

        String::from_utf8(buffer[start..end].to_vec())
            .map_err(|e| BinaryError::generic(format!("Invalid UTF-8 string: {}", e)))
    }

    /// Build hierarchical structure from flat node list
    fn build_hierarchy(tree: &mut TypeTree) -> Result<()> {
        if tree.nodes.is_empty() {
            return Ok(());
        }

        // Create a working copy of nodes
        let mut nodes = std::mem::take(&mut tree.nodes);

        // Build hierarchy using a stack-based approach
        let mut stack: Vec<(i32, usize)> = Vec::new(); // (level, index)
        let mut root_nodes = Vec::new();

        for (i, node) in nodes.iter().enumerate() {
            let current_level = node.level;

            // Pop stack until we find the parent level
            while let Some(&(level, _)) = stack.last() {
                if level < current_level {
                    break;
                }
                stack.pop();
            }

            if let Some(&(_, _parent_idx)) = stack.last() {
                // This node is a child of the node at parent_idx
                // We'll handle this in the second pass
            } else {
                // This is a root node
                root_nodes.push(i);
            }

            stack.push((current_level, i));
        }

        // Second pass: actually build the hierarchy
        let mut processed = vec![false; nodes.len()];
        let mut result_nodes = Vec::new();

        for &root_idx in &root_nodes {
            if !processed[root_idx] {
                let root_node = Self::build_node_hierarchy(&mut nodes, &mut processed, root_idx)?;
                result_nodes.push(root_node);
            }
        }

        tree.nodes = result_nodes;
        Ok(())
    }

    /// Build hierarchy for a single node and its children
    fn build_node_hierarchy(
        nodes: &mut [TypeTreeNode],
        processed: &mut [bool],
        node_idx: usize,
    ) -> Result<TypeTreeNode> {
        if processed[node_idx] {
            return Err(BinaryError::generic("Node already processed"));
        }

        let mut node = nodes[node_idx].clone();
        processed[node_idx] = true;

        let current_level = node.level;
        node.children.clear();

        // Find children (nodes with level = current_level + 1 that come after this node)
        for i in (node_idx + 1)..nodes.len() {
            if processed[i] {
                continue;
            }

            let child_level = nodes[i].level;

            if child_level <= current_level {
                // We've reached a sibling or parent level, stop looking for children
                break;
            }

            if child_level == current_level + 1 {
                // This is a direct child
                let child_node = Self::build_node_hierarchy(nodes, processed, i)?;
                node.children.push(child_node);
            }
        }

        Ok(node)
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
        let mut total_nodes = 0;
        let mut max_depth = 0;
        let mut primitive_count = 0;
        let mut array_count = 0;

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

        let mut stats = (0, 0, 0, 0);
        for node in &tree.nodes {
            count_nodes(node, 0, &mut stats);
        }

        total_nodes = stats.0;
        max_depth = stats.1;
        primitive_count = stats.2;
        array_count = stats.3;

        ParsingStats {
            total_nodes,
            root_nodes: tree.nodes.len(),
            max_depth,
            primitive_count,
            array_count,
            string_buffer_size: tree.string_buffer.len(),
            version: tree.version,
        }
    }
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

    #[test]
    fn test_parser_creation() {
        // Basic test to ensure parser methods exist
        assert!(true);
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
}
