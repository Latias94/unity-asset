//! TypeTree builder and validation
//!
//! This module provides functionality for building and validating TypeTree structures.

use super::types::{TypeTree, TypeTreeNode};
use crate::error::{BinaryError, Result};
use std::collections::HashMap;

/// TypeTree builder
///
/// This struct provides methods for building TypeTree structures programmatically,
/// including validation and optimization.
pub struct TypeTreeBuilder {
    tree: TypeTree,
    node_map: HashMap<String, usize>, // name -> node index for quick lookup
}

impl TypeTreeBuilder {
    /// Create a new TypeTree builder
    pub fn new() -> Self {
        Self {
            tree: TypeTree::new(),
            node_map: HashMap::new(),
        }
    }

    /// Create a builder with initial capacity
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            tree: TypeTree::with_capacity(capacity),
            node_map: HashMap::with_capacity(capacity),
        }
    }

    /// Set TypeTree version
    pub fn version(mut self, version: u32) -> Self {
        self.tree.version = version;
        self
    }

    /// Set platform
    pub fn platform(mut self, platform: u32) -> Self {
        self.tree.platform = platform;
        self
    }

    /// Set type dependencies flag
    pub fn has_type_dependencies(mut self, has_deps: bool) -> Self {
        self.tree.has_type_dependencies = has_deps;
        self
    }

    /// Add a root node
    pub fn add_root_node(&mut self, node: TypeTreeNode) -> Result<&mut Self> {
        if node.level != 0 {
            return Err(BinaryError::invalid_data("Root node must have level 0"));
        }

        let node_name = node.name.clone();
        let index = self.tree.nodes.len();

        self.tree.nodes.push(node);

        if !node_name.is_empty() {
            self.node_map.insert(node_name, index);
        }

        Ok(self)
    }

    /// Create and add a simple node
    pub fn add_simple_node(
        &mut self,
        type_name: String,
        name: String,
        byte_size: i32,
        level: i32,
    ) -> Result<&mut Self> {
        let mut node = TypeTreeNode::new();
        node.type_name = type_name;
        node.name = name.clone();
        node.byte_size = byte_size;
        node.level = level;
        node.index = self.tree.nodes.len() as i32;

        if level == 0 {
            self.add_root_node(node)?;
        } else {
            return Err(BinaryError::invalid_data(
                "Use add_child_to_node for non-root nodes",
            ));
        }

        Ok(self)
    }

    /// Add a child node to an existing node
    pub fn add_child_to_node(
        &mut self,
        parent_name: &str,
        child: TypeTreeNode,
    ) -> Result<&mut Self> {
        let parent_index = self.node_map.get(parent_name).copied().ok_or_else(|| {
            BinaryError::generic(format!("Parent node '{}' not found", parent_name))
        })?;

        // Validate child level
        let parent_level = self.tree.nodes[parent_index].level;
        if child.level != parent_level + 1 {
            return Err(BinaryError::invalid_data(format!(
                "Child level must be parent level + 1 (expected {}, got {})",
                parent_level + 1,
                child.level
            )));
        }

        let child_name = child.name.clone();
        self.tree.nodes[parent_index].children.push(child);

        // Update node map if child has a name
        if !child_name.is_empty() {
            let child_index = self.tree.nodes[parent_index].children.len() - 1;
            self.node_map
                .insert(format!("{}.{}", parent_name, child_name), child_index);
        }

        Ok(self)
    }

    /// Build common primitive types
    pub fn add_primitive_field(
        &mut self,
        parent_name: &str,
        field_name: String,
        type_name: &str,
    ) -> Result<&mut Self> {
        let parent_index = self.node_map.get(parent_name).copied().ok_or_else(|| {
            BinaryError::generic(format!("Parent node '{}' not found", parent_name))
        })?;

        let parent_level = self.tree.nodes[parent_index].level;
        let byte_size = Self::get_primitive_size(type_name)?;

        let mut child = TypeTreeNode::new();
        child.type_name = type_name.to_string();
        child.name = field_name;
        child.byte_size = byte_size;
        child.level = parent_level + 1;
        child.index = (self.tree.nodes.len() + self.tree.nodes[parent_index].children.len()) as i32;

        self.add_child_to_node(parent_name, child)?;
        Ok(self)
    }

    /// Get the size of primitive types
    fn get_primitive_size(type_name: &str) -> Result<i32> {
        let size = match type_name {
            "bool" | "SInt8" | "UInt8" | "char" => 1,
            "SInt16" | "UInt16" | "short" | "unsigned short" => 2,
            "SInt32" | "UInt32" | "int" | "unsigned int" | "float" => 4,
            "SInt64" | "UInt64" | "long long" | "unsigned long long" | "double" => 8,
            "string" => -1, // Variable size
            _ => {
                return Err(BinaryError::invalid_data(format!(
                    "Unknown primitive type: {}",
                    type_name
                )));
            }
        };
        Ok(size)
    }

    /// Add an array field
    pub fn add_array_field(
        &mut self,
        parent_name: &str,
        field_name: String,
        element_type: &str,
    ) -> Result<&mut Self> {
        let parent_index = self.node_map.get(parent_name).copied().ok_or_else(|| {
            BinaryError::generic(format!("Parent node '{}' not found", parent_name))
        })?;

        let parent_level = self.tree.nodes[parent_index].level;

        // Create array container node
        let mut array_node = TypeTreeNode::new();
        array_node.type_name = "Array".to_string();
        array_node.name = field_name.clone();
        array_node.byte_size = -1; // Variable size
        array_node.level = parent_level + 1;
        array_node.index =
            (self.tree.nodes.len() + self.tree.nodes[parent_index].children.len()) as i32;

        // Create size node
        let mut size_node = TypeTreeNode::new();
        size_node.type_name = "int".to_string();
        size_node.name = "size".to_string();
        size_node.byte_size = 4;
        size_node.level = parent_level + 2;
        size_node.index = array_node.index + 1;

        // Create data array node
        let mut data_node = TypeTreeNode::new();
        data_node.type_name = format!("Array<{}>", element_type);
        data_node.name = "data".to_string();
        data_node.byte_size = -1; // Variable size
        data_node.level = parent_level + 2;
        data_node.index = array_node.index + 2;

        // Create element node
        let mut element_node = TypeTreeNode::new();
        element_node.type_name = element_type.to_string();
        element_node.name = String::new(); // Array elements don't have names
        element_node.byte_size = Self::get_primitive_size(element_type).unwrap_or(-1);
        element_node.level = parent_level + 3;
        element_node.index = array_node.index + 3;

        // Build hierarchy
        data_node.children.push(element_node);
        array_node.children.push(size_node);
        array_node.children.push(data_node);

        self.add_child_to_node(parent_name, array_node)?;
        Ok(self)
    }

    /// Validate the built TypeTree
    pub fn validate(&self) -> Result<()> {
        self.tree.validate().map_err(BinaryError::generic)
    }

    /// Build and return the TypeTree
    pub fn build(mut self) -> Result<TypeTree> {
        // Final validation
        self.validate()?;

        // Update string buffer
        self.update_string_buffer();

        // Update node indices
        self.update_node_indices();

        Ok(self.tree)
    }

    /// Update the string buffer with all type and field names
    fn update_string_buffer(&mut self) {
        self.tree.string_buffer.clear();

        // Collect all unique strings
        let mut strings = std::collections::HashSet::new();
        Self::collect_strings(&self.tree.nodes, &mut strings);

        // Build string buffer and update offsets
        let mut offset_map = HashMap::new();
        for string in &strings {
            let offset = self.tree.string_buffer.len() as u32;
            offset_map.insert(string.clone(), offset);
            self.tree.string_buffer.extend_from_slice(string.as_bytes());
            self.tree.string_buffer.push(0); // Null terminator
        }

        // Update node offsets
        Self::update_string_offsets(&mut self.tree.nodes, &offset_map);
    }

    /// Collect all strings from nodes
    fn collect_strings(nodes: &[TypeTreeNode], strings: &mut std::collections::HashSet<String>) {
        for node in nodes {
            if !node.type_name.is_empty() {
                strings.insert(node.type_name.clone());
            }
            if !node.name.is_empty() {
                strings.insert(node.name.clone());
            }
            Self::collect_strings(&node.children, strings);
        }
    }

    /// Update string offsets in nodes
    fn update_string_offsets(nodes: &mut [TypeTreeNode], offset_map: &HashMap<String, u32>) {
        for node in nodes {
            if let Some(&offset) = offset_map.get(&node.type_name) {
                node.type_str_offset = offset;
            }
            if let Some(&offset) = offset_map.get(&node.name) {
                node.name_str_offset = offset;
            }
            Self::update_string_offsets(&mut node.children, offset_map);
        }
    }

    /// Update node indices
    fn update_node_indices(&mut self) {
        let mut index = 0;
        Self::update_indices(&mut self.tree.nodes, &mut index);
    }

    /// Update indices recursively
    fn update_indices(nodes: &mut [TypeTreeNode], index: &mut i32) {
        for node in nodes {
            node.index = *index;
            *index += 1;
            Self::update_indices(&mut node.children, index);
        }
    }

    /// Get the current tree (for inspection during building)
    pub fn tree(&self) -> &TypeTree {
        &self.tree
    }

    /// Get mutable access to the current tree
    pub fn tree_mut(&mut self) -> &mut TypeTree {
        &mut self.tree
    }
}

impl Default for TypeTreeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// TypeTree validator
pub struct TypeTreeValidator;

impl TypeTreeValidator {
    /// Validate a complete TypeTree
    pub fn validate(tree: &TypeTree) -> Result<ValidationReport> {
        let mut report = ValidationReport::new();

        // Basic structure validation
        if tree.nodes.is_empty() {
            report.add_error("TypeTree has no nodes".to_string());
            return Ok(report);
        }

        // Validate each root node
        for (i, node) in tree.nodes.iter().enumerate() {
            Self::validate_node(node, 0, &mut report, &format!("root[{}]", i));
        }

        // Validate string buffer
        Self::validate_string_buffer(tree, &mut report);

        Ok(report)
    }

    /// Validate a single node
    fn validate_node(
        node: &TypeTreeNode,
        expected_level: i32,
        report: &mut ValidationReport,
        path: &str,
    ) {
        // Check level
        if node.level != expected_level {
            report.add_error(format!(
                "{}: Level mismatch (expected {}, got {})",
                path, expected_level, node.level
            ));
        }

        // Check type name
        if node.type_name.is_empty() {
            report.add_error(format!("{}: Empty type name", path));
        }

        // Check byte size
        if node.byte_size < -1 {
            report.add_error(format!("{}: Invalid byte size ({})", path, node.byte_size));
        }

        // Validate children
        for (i, child) in node.children.iter().enumerate() {
            let child_path = if node.name.is_empty() {
                format!("{}[{}]", path, i)
            } else {
                format!("{}.{}", path, child.name)
            };
            Self::validate_node(child, expected_level + 1, report, &child_path);
        }
    }

    /// Validate string buffer
    fn validate_string_buffer(tree: &TypeTree, report: &mut ValidationReport) {
        // Check if string buffer is properly null-terminated
        if !tree.string_buffer.is_empty() && tree.string_buffer[tree.string_buffer.len() - 1] != 0 {
            report.add_warning("String buffer is not null-terminated".to_string());
        }

        // Validate string offsets
        Self::validate_string_offsets(&tree.nodes, &tree.string_buffer, report, "root");
    }

    /// Validate string offsets in nodes
    fn validate_string_offsets(
        nodes: &[TypeTreeNode],
        string_buffer: &[u8],
        report: &mut ValidationReport,
        path: &str,
    ) {
        for (i, node) in nodes.iter().enumerate() {
            let node_path = format!("{}[{}]", path, i);

            // Check type string offset
            if node.type_str_offset as usize >= string_buffer.len() {
                report.add_error(format!(
                    "{}: Type string offset out of bounds ({})",
                    node_path, node.type_str_offset
                ));
            }

            // Check name string offset
            if node.name_str_offset as usize >= string_buffer.len() {
                report.add_error(format!(
                    "{}: Name string offset out of bounds ({})",
                    node_path, node.name_str_offset
                ));
            }

            // Validate children
            Self::validate_string_offsets(&node.children, string_buffer, report, &node_path);
        }
    }
}

/// Validation report
#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl ValidationReport {
    pub fn new() -> Self {
        Self {
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn add_error(&mut self, error: String) {
        self.errors.push(error);
    }

    pub fn add_warning(&mut self, warning: String) {
        self.warnings.push(warning);
    }

    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }
}

impl Default for ValidationReport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_creation() {
        let builder = TypeTreeBuilder::new();
        assert!(builder.tree().is_empty());
    }

    #[test]
    fn test_primitive_sizes() {
        assert_eq!(TypeTreeBuilder::get_primitive_size("int").unwrap(), 4);
        assert_eq!(TypeTreeBuilder::get_primitive_size("bool").unwrap(), 1);
        assert_eq!(TypeTreeBuilder::get_primitive_size("double").unwrap(), 8);
        assert_eq!(TypeTreeBuilder::get_primitive_size("string").unwrap(), -1);
    }
}
