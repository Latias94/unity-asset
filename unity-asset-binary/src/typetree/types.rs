//! TypeTree data structures
//!
//! This module defines the core data structures for Unity TypeTree processing.
//! TypeTree provides dynamic type information for Unity objects.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A node in the Unity TypeTree
/// 
/// Each node represents a field or type in the Unity object structure,
/// forming a tree that describes the complete object layout.
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

    /// Create a new node with basic information
    pub fn with_info(type_name: String, name: String, byte_size: i32) -> Self {
        Self {
            type_name,
            name,
            byte_size,
            ..Default::default()
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

    /// Check if this is a string type
    pub fn is_string(&self) -> bool {
        self.type_name == "string"
    }

    /// Check if this is a numeric type
    pub fn is_numeric(&self) -> bool {
        matches!(
            self.type_name.as_str(),
            "SInt8" | "UInt8" | "SInt16" | "UInt16" | "SInt32" | "UInt32" 
            | "SInt64" | "UInt64" | "float" | "double" | "int"
        )
    }

    /// Check if this is a boolean type
    pub fn is_boolean(&self) -> bool {
        self.type_name == "bool"
    }

    /// Find a child node by name
    pub fn find_child(&self, name: &str) -> Option<&TypeTreeNode> {
        self.children.iter().find(|child| child.name == name)
    }

    /// Find a child node by name (mutable)
    pub fn find_child_mut(&mut self, name: &str) -> Option<&mut TypeTreeNode> {
        self.children.iter_mut().find(|child| child.name == name)
    }

    /// Get all child names
    pub fn child_names(&self) -> Vec<&str> {
        self.children.iter().map(|child| child.name.as_str()).collect()
    }

    /// Add a child node
    pub fn add_child(&mut self, child: TypeTreeNode) {
        self.children.push(child);
    }

    /// Remove a child node by name
    pub fn remove_child(&mut self, name: &str) -> Option<TypeTreeNode> {
        if let Some(pos) = self.children.iter().position(|child| child.name == name) {
            Some(self.children.remove(pos))
        } else {
            None
        }
    }

    /// Get the depth of this node in the tree
    pub fn depth(&self) -> i32 {
        self.level
    }

    /// Check if this node has children
    pub fn has_children(&self) -> bool {
        !self.children.is_empty()
    }

    /// Get the number of children
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Validate the node structure
    pub fn validate(&self) -> Result<(), String> {
        if self.type_name.is_empty() {
            return Err("Type name cannot be empty".to_string());
        }

        if self.byte_size < -1 {
            return Err("Invalid byte size".to_string());
        }

        // Validate children
        for (i, child) in self.children.iter().enumerate() {
            child.validate().map_err(|e| format!("Child {}: {}", i, e))?;
        }

        Ok(())
    }
}

impl Default for TypeTreeNode {
    fn default() -> Self {
        Self::new()
    }
}

/// Complete TypeTree structure
/// 
/// This structure contains the complete type information for a Unity object,
/// including all field definitions and their relationships.
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

    /// Create a TypeTree with initial capacity
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            nodes: Vec::with_capacity(capacity),
            string_buffer: Vec::new(),
            version: 0,
            platform: 0,
            has_type_dependencies: false,
        }
    }

    /// Check if the TypeTree is empty
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Get the number of root nodes
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Add a root node
    pub fn add_node(&mut self, node: TypeTreeNode) {
        self.nodes.push(node);
    }

    /// Find a root node by name
    pub fn find_node(&self, name: &str) -> Option<&TypeTreeNode> {
        self.nodes.iter().find(|node| node.name == name)
    }

    /// Find a root node by name (mutable)
    pub fn find_node_mut(&mut self, name: &str) -> Option<&mut TypeTreeNode> {
        self.nodes.iter_mut().find(|node| node.name == name)
    }

    /// Get all root node names
    pub fn node_names(&self) -> Vec<&str> {
        self.nodes.iter().map(|node| node.name.as_str()).collect()
    }

    /// Clear all nodes
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.string_buffer.clear();
    }

    /// Get string from buffer at offset
    pub fn get_string(&self, offset: u32) -> Option<String> {
        if offset as usize >= self.string_buffer.len() {
            return None;
        }

        let start = offset as usize;
        let end = self.string_buffer[start..]
            .iter()
            .position(|&b| b == 0)
            .map(|pos| start + pos)
            .unwrap_or(self.string_buffer.len());

        String::from_utf8(self.string_buffer[start..end].to_vec()).ok()
    }

    /// Add string to buffer and return offset
    pub fn add_string(&mut self, s: &str) -> u32 {
        let offset = self.string_buffer.len() as u32;
        self.string_buffer.extend_from_slice(s.as_bytes());
        self.string_buffer.push(0); // Null terminator
        offset
    }

    /// Validate the entire TypeTree
    pub fn validate(&self) -> Result<(), String> {
        if self.nodes.is_empty() {
            return Err("TypeTree has no nodes".to_string());
        }

        for (i, node) in self.nodes.iter().enumerate() {
            node.validate().map_err(|e| format!("Root node {}: {}", i, e))?;
        }

        Ok(())
    }

    /// Get TypeTree statistics
    pub fn statistics(&self) -> TypeTreeStatistics {
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
        for node in &self.nodes {
            count_nodes(node, 0, &mut stats);
        }

        total_nodes = stats.0;
        max_depth = stats.1;
        primitive_count = stats.2;
        array_count = stats.3;

        TypeTreeStatistics {
            total_nodes,
            root_nodes: self.nodes.len(),
            max_depth,
            primitive_count,
            array_count,
            string_buffer_size: self.string_buffer.len(),
        }
    }
}

impl Default for TypeTree {
    fn default() -> Self {
        Self::new()
    }
}

/// TypeTree statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeTreeStatistics {
    pub total_nodes: usize,
    pub root_nodes: usize,
    pub max_depth: i32,
    pub primitive_count: usize,
    pub array_count: usize,
    pub string_buffer_size: usize,
}

/// Type information for Unity classes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeInfo {
    pub class_id: i32,
    pub class_name: String,
    pub type_tree: TypeTree,
    pub script_type_index: Option<i16>,
    pub script_id: [u8; 16],
    pub old_type_hash: [u8; 16],
}

impl TypeInfo {
    /// Create new type info
    pub fn new(class_id: i32, class_name: String) -> Self {
        Self {
            class_id,
            class_name,
            type_tree: TypeTree::new(),
            script_type_index: None,
            script_id: [0; 16],
            old_type_hash: [0; 16],
        }
    }

    /// Check if this is a script type
    pub fn is_script_type(&self) -> bool {
        self.script_type_index.is_some()
    }
}

/// Type registry for managing multiple types
#[derive(Debug, Clone, Default)]
pub struct TypeRegistry {
    types: HashMap<i32, TypeInfo>,
}

impl TypeRegistry {
    /// Create a new type registry
    pub fn new() -> Self {
        Self {
            types: HashMap::new(),
        }
    }

    /// Add a type to the registry
    pub fn add_type(&mut self, type_info: TypeInfo) {
        self.types.insert(type_info.class_id, type_info);
    }

    /// Get a type by class ID
    pub fn get_type(&self, class_id: i32) -> Option<&TypeInfo> {
        self.types.get(&class_id)
    }

    /// Get all registered class IDs
    pub fn class_ids(&self) -> Vec<i32> {
        self.types.keys().copied().collect()
    }

    /// Check if a class ID is registered
    pub fn has_type(&self, class_id: i32) -> bool {
        self.types.contains_key(&class_id)
    }

    /// Clear all types
    pub fn clear(&mut self) {
        self.types.clear();
    }

    /// Get the number of registered types
    pub fn len(&self) -> usize {
        self.types.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }
}
