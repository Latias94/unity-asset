//! TypeTree parsing for Unity binary files
//!
//! TypeTree provides dynamic type information for Unity objects,
//! allowing parsing of objects without prior knowledge of their structure.

use crate::error::{BinaryError, Result};
use crate::reader::BinaryReader;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use unity_asset_core::UnityValue;

/// A node in the Unity TypeTree
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeTreeNode {
    /// Type name (e.g., "int", "string", "GameObject")
    pub type_name: String,
    /// Field name (e.g., "m_Name", "m_IsActive")
    pub name: String,
    /// Size in bytes (-1 for variable size)
    pub byte_size: i32,
    /// Index in the type tree
    pub index: i32,
    /// Type flags
    pub type_flags: i32,
    /// Version of this type
    pub version: i32,
    /// Meta flags (alignment, etc.)
    pub meta_flags: i32,
    /// Depth level in the tree
    pub level: i32,
    /// Offset in type string buffer
    pub type_str_offset: u32,
    /// Offset in name string buffer
    pub name_str_offset: u32,
    /// Reference type hash
    pub ref_type_hash: u64,
    /// Child nodes
    pub children: Vec<TypeTreeNode>,
}

impl TypeTreeNode {
    /// Create a new TypeTree node
    pub fn new() -> Self {
        Self {
            type_name: String::new(),
            name: String::new(),
            byte_size: 0,
            index: 0,
            type_flags: 0,
            version: 0,
            meta_flags: 0,
            level: 0,
            type_str_offset: 0,
            name_str_offset: 0,
            ref_type_hash: 0,
            children: Vec::new(),
        }
    }

    /// Check if this node represents an array
    pub fn is_array(&self) -> bool {
        self.type_name == "Array" || self.type_name.starts_with("vector")
    }

    /// Check if this node is aligned
    pub fn is_aligned(&self) -> bool {
        (self.meta_flags & 0x4000) != 0
    }

    /// Get the size of this type
    pub fn size(&self) -> i32 {
        self.byte_size
    }

    /// Check if this is a primitive type
    pub fn is_primitive(&self) -> bool {
        matches!(
            self.type_name.as_str(),
            "bool"
                | "char"
                | "SInt8"
                | "UInt8"
                | "SInt16"
                | "UInt16"
                | "SInt32"
                | "UInt32"
                | "SInt64"
                | "UInt64"
                | "float"
                | "double"
                | "int"
                | "string"
        )
    }

    /// Find a child node by name
    pub fn find_child(&self, name: &str) -> Option<&TypeTreeNode> {
        self.children.iter().find(|child| child.name == name)
    }

    /// Find a child node by name (mutable)
    pub fn find_child_mut(&mut self, name: &str) -> Option<&mut TypeTreeNode> {
        self.children.iter_mut().find(|child| child.name == name)
    }
}

impl Default for TypeTreeNode {
    fn default() -> Self {
        Self::new()
    }
}

/// Complete TypeTree structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeTree {
    /// Root nodes of the type tree
    pub nodes: Vec<TypeTreeNode>,
    /// String buffer for type and field names
    pub string_buffer: Vec<u8>,
    /// Version of the type tree format
    pub version: u32,
    /// Platform target
    pub platform: u32,
    /// Whether type tree has type dependencies
    pub has_type_dependencies: bool,
}

impl TypeTree {
    /// Create a new empty TypeTree
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            string_buffer: Vec::new(),
            version: 0,
            platform: 0,
            has_type_dependencies: false,
        }
    }

    /// Parse TypeTree from binary data
    pub fn from_reader(reader: &mut BinaryReader, version: u32) -> Result<Self> {
        let mut tree = Self::new();
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
        tree.resolve_strings()?;

        // Build tree hierarchy
        tree.build_hierarchy()?;

        Ok(tree)
    }

    /// Parse TypeTree from binary data using blob format (Unity version >= 12 or == 10)
    pub fn from_reader_blob(reader: &mut BinaryReader, version: u32) -> Result<Self> {
        let mut tree = Self::new();
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
        tree.resolve_strings()?;

        // Build tree hierarchy
        tree.build_hierarchy()?;

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

    /// Resolve string references using the string buffer
    fn resolve_strings(&mut self) -> Result<()> {
        // Collect the string offsets first to avoid borrowing issues
        let string_data: Vec<(u32, u32)> = self
            .nodes
            .iter()
            .map(|node| (node.type_str_offset, node.name_str_offset))
            .collect();

        for (i, (type_offset, name_offset)) in string_data.iter().enumerate() {
            let type_name = self.get_string(*type_offset)?;
            let name = self.get_string(*name_offset)?;

            self.nodes[i].type_name = type_name;
            self.nodes[i].name = name;
        }
        Ok(())
    }

    /// Get a string from the string buffer at the given offset
    fn get_string(&self, offset: u32) -> Result<String> {
        let offset = offset as usize;
        if offset >= self.string_buffer.len() {
            return Ok(String::new());
        }

        // Find null terminator
        let end = self.string_buffer[offset..]
            .iter()
            .position(|&b| b == 0)
            .map(|pos| offset + pos)
            .unwrap_or(self.string_buffer.len());

        let bytes = &self.string_buffer[offset..end];
        Ok(String::from_utf8(bytes.to_vec())?)
    }

    /// Build hierarchical structure from flat node list
    fn build_hierarchy(&mut self) -> Result<()> {
        if self.nodes.is_empty() {
            return Ok(());
        }

        // Create a stack to track parent nodes at each level
        let mut parent_stack: Vec<usize> = Vec::new();
        let mut root_indices = Vec::new();

        for i in 0..self.nodes.len() {
            let level = self.nodes[i].level;

            // Pop parents that are at the same or deeper level
            while let Some(&parent_idx) = parent_stack.last() {
                if self.nodes[parent_idx].level < level {
                    break;
                }
                parent_stack.pop();
            }

            if let Some(&_parent_idx) = parent_stack.last() {
                // This node is a child of the current parent
                // We'll handle this in a second pass since we can't move nodes while iterating
            } else {
                // This is a root node
                root_indices.push(i);
            }

            parent_stack.push(i);
        }

        // Second pass: actually build the hierarchy
        // This is complex due to Rust's ownership rules, so we'll use indices
        self.build_hierarchy_recursive()?;

        Ok(())
    }

    /// Build hierarchical relationships between nodes
    fn build_hierarchy_recursive(&mut self) -> Result<()> {
        if self.nodes.is_empty() {
            return Ok(());
        }

        // Create a hierarchical structure from flat nodes
        // We'll build a tree by processing nodes in order and using their level information

        // First, collect parent-child relationships
        let mut parent_child_map: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        let mut level_stack: Vec<usize> = Vec::new();

        for i in 0..self.nodes.len() {
            let current_level = self.nodes[i].level;

            // Remove parents that are at the same level or deeper
            while let Some(&parent_idx) = level_stack.last() {
                if self.nodes[parent_idx].level < current_level {
                    break;
                }
                level_stack.pop();
            }

            // If we have a parent, this node is a child
            if let Some(&parent_idx) = level_stack.last() {
                parent_child_map.entry(parent_idx).or_default().push(i);
            }

            // Add current node to the stack as a potential parent
            level_stack.push(i);
        }

        // Now build the actual hierarchy by cloning nodes and setting up children
        // We need to do this carefully to avoid borrowing issues
        let original_nodes = self.nodes.clone();

        for (parent_idx, child_indices) in parent_child_map {
            let mut children = Vec::new();
            for child_idx in child_indices {
                children.push(original_nodes[child_idx].clone());
            }
            self.nodes[parent_idx].children = children;
        }

        Ok(())
    }

    /// Find the root node (usually at level 0)
    pub fn root(&self) -> Option<&TypeTreeNode> {
        self.nodes.iter().find(|node| node.level == 0)
    }

    /// Get all nodes at a specific level
    pub fn nodes_at_level(&self, level: i32) -> Vec<&TypeTreeNode> {
        self.nodes
            .iter()
            .filter(|node| node.level == level)
            .collect()
    }

    /// Find a node by name
    pub fn find_node(&self, name: &str) -> Option<&TypeTreeNode> {
        self.nodes.iter().find(|node| node.name == name)
    }

    /// Get type information as a map
    pub fn to_type_map(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for node in &self.nodes {
            if !node.name.is_empty() {
                map.insert(node.name.clone(), node.type_name.clone());
            }
        }
        map
    }

    /// Parse TypeTree as dictionary (raw data structure)
    pub fn parse_as_dict(&self, reader: &mut BinaryReader) -> Result<IndexMap<String, UnityValue>> {
        let mut properties = IndexMap::new();

        if let Some(root) = self.root() {
            // For root node, parse its children as top-level properties
            for child in &root.children {
                if !child.name.is_empty() {
                    match self.parse_value_by_type(reader, child) {
                        Ok(value) => {
                            properties.insert(child.name.clone(), value);
                        }
                        Err(e) => {
                            // Check if this is a critical error (insufficient data)
                            match &e {
                                BinaryError::NotEnoughData { .. } => {
                                    // For data reading errors, fail immediately
                                    return Err(e);
                                }
                                _ => {
                                    // For other errors, insert null value as placeholder
                                    properties.insert(child.name.clone(), UnityValue::Null);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(properties)
    }

    /// Parse value based on TypeTree node type
    fn parse_value_by_type(
        &self,
        reader: &mut BinaryReader,
        node: &TypeTreeNode,
    ) -> Result<UnityValue> {
        // Handle alignment if needed
        if self.is_aligned(node) {
            reader.align_to(4)?;
        }

        let value = match node.type_name.as_str() {
            // Signed integers
            "SInt8" | "char" => {
                let val = reader.read_i8()?;
                // Align after reading 1-byte values
                reader.align_to(4)?;
                UnityValue::Integer(val as i64)
            }
            "SInt16" | "short" => {
                let val = reader.read_i16()?;
                // Align after reading 2-byte values
                reader.align_to(4)?;
                UnityValue::Integer(val as i64)
            }
            "SInt32" | "int" => {
                let val = reader.read_i32()?;
                UnityValue::Integer(val as i64)
            }
            "SInt64" | "long long" => {
                let val = reader.read_i64()?;
                UnityValue::Integer(val)
            }

            // Unsigned integers
            "UInt8" => {
                let val = reader.read_u8()?;
                // Align after reading 1-byte values
                reader.align_to(4)?;
                UnityValue::Integer(val as i64)
            }
            "UInt16" | "unsigned short" => {
                let val = reader.read_u16()?;
                // Align after reading 2-byte values
                reader.align_to(4)?;
                UnityValue::Integer(val as i64)
            }
            "UInt32" | "unsigned int" | "Type*" => {
                let val = reader.read_u32()?;
                UnityValue::Integer(val as i64)
            }
            "UInt64" | "unsigned long long" | "FileSize" => {
                let val = reader.read_u64()?;
                UnityValue::Integer(val as i64)
            }

            // Floating point
            "float" => {
                let val = reader.read_f32()?;
                UnityValue::Float(val as f64)
            }
            "double" => {
                let val = reader.read_f64()?;
                UnityValue::Float(val)
            }

            // Boolean
            "bool" => {
                let val = reader.read_u8()? != 0;
                // Align after reading boolean
                reader.align_to(4)?;
                UnityValue::Bool(val)
            }

            // String
            "string" => {
                let val = reader.read_string()?;
                // Align after reading string
                reader.align_to(4)?;
                UnityValue::String(val)
            }

            // Array types - check if this node has Array child
            _ if !node.children.is_empty()
                && node.children.iter().any(|c| c.type_name == "Array") =>
            {
                self.parse_array(reader, node)?
            }

            // Pair type
            "pair" if node.children.len() == 2 => {
                let first = self.parse_value_by_type(reader, &node.children[0])?;
                let second = self.parse_value_by_type(reader, &node.children[1])?;
                UnityValue::Array(vec![first, second])
            }

            // Complex object types
            _ => {
                if !node.children.is_empty() {
                    let mut nested_props = IndexMap::new();
                    for child in &node.children {
                        if !child.name.is_empty() {
                            let child_value = self.parse_value_by_type(reader, child)?;
                            nested_props.insert(child.name.clone(), child_value);
                        }
                    }
                    UnityValue::Object(nested_props)
                } else {
                    // Unknown primitive type, return null
                    UnityValue::Null
                }
            }
        };

        Ok(value)
    }

    /// Parse array from TypeTree node
    fn parse_array(&self, reader: &mut BinaryReader, node: &TypeTreeNode) -> Result<UnityValue> {
        // Find the Array child node
        let array_node = node
            .children
            .iter()
            .find(|child| child.type_name == "Array")
            .ok_or_else(|| BinaryError::invalid_data("Array node not found".to_string()))?;

        if array_node.children.len() < 2 {
            return Ok(UnityValue::Array(Vec::new()));
        }

        // Read array size (first child is size)
        let size = reader.read_i32()? as usize;
        if size > 1_000_000 {
            // Sanity check to prevent memory exhaustion
            return Err(BinaryError::invalid_data(format!(
                "Array size too large: {}",
                size
            )));
        }

        // Second child is the element type
        let element_node = &array_node.children[1];
        let mut elements = Vec::with_capacity(size);

        // Handle alignment for array elements
        if self.is_aligned(element_node) {
            reader.align_to(4)?;
        }

        for _ in 0..size {
            let element = self.parse_value_by_type(reader, element_node)?;
            elements.push(element);
        }

        Ok(UnityValue::Array(elements))
    }

    /// Check if a node requires alignment
    pub fn is_aligned(&self, node: &TypeTreeNode) -> bool {
        const ALIGN_BYTES: i32 = 0x4000;
        (node.meta_flags & ALIGN_BYTES) != 0
    }
}

impl Default for TypeTree {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Removed unused import

    #[test]
    fn test_typetree_node_creation() {
        let node = TypeTreeNode::new();
        assert_eq!(node.type_name, "");
        assert_eq!(node.name, "");
        assert_eq!(node.level, 0);
    }

    #[test]
    fn test_typetree_node_is_primitive() {
        let mut node = TypeTreeNode::new();

        node.type_name = "int".to_string();
        assert!(node.is_primitive());

        node.type_name = "GameObject".to_string();
        assert!(!node.is_primitive());
    }

    #[test]
    fn test_typetree_node_is_array() {
        let mut node = TypeTreeNode::new();

        node.type_name = "Array".to_string();
        assert!(node.is_array());

        node.type_name = "vector".to_string();
        assert!(node.is_array());

        node.type_name = "int".to_string();
        assert!(!node.is_array());
    }

    #[test]
    fn test_typetree_creation() {
        let tree = TypeTree::new();
        assert_eq!(tree.nodes.len(), 0);
        assert_eq!(tree.string_buffer.len(), 0);
        assert_eq!(tree.version, 0);
    }

    #[test]
    fn test_string_resolution() {
        let mut tree = TypeTree::new();
        tree.string_buffer = b"int\0float\0GameObject\0".to_vec();

        assert_eq!(tree.get_string(0).unwrap(), "int");
        assert_eq!(tree.get_string(4).unwrap(), "float");
        assert_eq!(tree.get_string(10).unwrap(), "GameObject");
    }
}
