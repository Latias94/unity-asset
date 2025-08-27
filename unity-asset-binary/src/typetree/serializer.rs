//! TypeTree serialization and deserialization
//!
//! This module provides functionality for serializing and deserializing
//! Unity objects using TypeTree information.

use super::types::{TypeTree, TypeTreeNode};
use crate::error::{BinaryError, Result};
use crate::reader::BinaryReader;
use indexmap::IndexMap;
use unity_asset_core::UnityValue;

/// TypeTree serializer
///
/// This struct provides methods for serializing and deserializing Unity objects
/// using TypeTree structure information.
pub struct TypeTreeSerializer<'a> {
    tree: &'a TypeTree,
}

impl<'a> TypeTreeSerializer<'a> {
    /// Create a new serializer with a TypeTree
    pub fn new(tree: &'a TypeTree) -> Self {
        Self { tree }
    }

    /// Parse object data using the TypeTree structure
    pub fn parse_object(&self, reader: &mut BinaryReader) -> Result<IndexMap<String, UnityValue>> {
        let mut properties = IndexMap::new();

        if let Some(root) = self.tree.nodes.first() {
            // For root node, parse its children as top-level properties
            for child in &root.children {
                if !child.name.is_empty() {
                    match self.parse_value_by_type(reader, child) {
                        Ok(value) => {
                            properties.insert(child.name.clone(), value);
                        }
                        Err(e) => {
                            // Check if this is a critical error (insufficient data)
                            if reader.remaining() == 0 {
                                // No more data to read, this is expected for some objects
                                break;
                            }
                            // For other errors, we might want to continue or fail
                            // depending on the use case. For now, we'll continue.
                            eprintln!("Warning: Failed to parse field '{}': {}", child.name, e);
                            continue;
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
        if node.is_aligned() {
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

            // Array types
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
                    // Unknown type with no children, skip bytes if size is known
                    if node.byte_size > 0 {
                        let _data = reader.read_bytes(node.byte_size as usize)?;
                        UnityValue::Null
                    } else {
                        UnityValue::Null
                    }
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
            .ok_or_else(|| BinaryError::invalid_data("Array node not found in array type"))?;

        // Read array size (first child is size)
        let size = reader.read_i32()? as usize;
        if size > 1_000_000 {
            // Sanity check to prevent memory exhaustion
            return Err(BinaryError::invalid_data(format!(
                "Array size too large: {}",
                size
            )));
        }

        let mut elements = Vec::with_capacity(size);

        // Find the element type (usually the second child of Array node)
        let element_node = array_node
            .children
            .get(1)
            .ok_or_else(|| BinaryError::invalid_data("Array element type not found"))?;

        for _ in 0..size {
            let element = self.parse_value_by_type(reader, element_node)?;
            elements.push(element);
        }

        Ok(UnityValue::Array(elements))
    }

    /// Serialize object data using the TypeTree structure
    pub fn serialize_object(&self, data: &IndexMap<String, UnityValue>) -> Result<Vec<u8>> {
        let mut buffer = Vec::new();

        if let Some(root) = self.tree.nodes.first() {
            for child in &root.children {
                if !child.name.is_empty()
                    && let Some(value) = data.get(&child.name)
                {
                    self.serialize_value(&mut buffer, value, child)?;
                }
            }
        }

        Ok(buffer)
    }

    /// Serialize a single value based on TypeTree node type
    fn serialize_value(
        &self,
        buffer: &mut Vec<u8>,
        value: &UnityValue,
        node: &TypeTreeNode,
    ) -> Result<()> {
        match node.type_name.as_str() {
            "SInt8" | "char" => {
                if let UnityValue::Integer(val) = value {
                    buffer.push(*val as u8);
                    self.align_buffer(buffer, 4);
                }
            }
            "SInt16" | "short" => {
                if let UnityValue::Integer(val) = value {
                    buffer.extend_from_slice(&(*val as i16).to_le_bytes());
                    self.align_buffer(buffer, 4);
                }
            }
            "SInt32" | "int" => {
                if let UnityValue::Integer(val) = value {
                    buffer.extend_from_slice(&(*val as i32).to_le_bytes());
                }
            }
            "SInt64" | "long long" => {
                if let UnityValue::Integer(val) = value {
                    buffer.extend_from_slice(&val.to_le_bytes());
                }
            }
            "UInt8" => {
                if let UnityValue::Integer(val) = value {
                    buffer.push(*val as u8);
                    self.align_buffer(buffer, 4);
                }
            }
            "UInt16" | "unsigned short" => {
                if let UnityValue::Integer(val) = value {
                    buffer.extend_from_slice(&(*val as u16).to_le_bytes());
                    self.align_buffer(buffer, 4);
                }
            }
            "UInt32" | "unsigned int" | "Type*" => {
                if let UnityValue::Integer(val) = value {
                    buffer.extend_from_slice(&(*val as u32).to_le_bytes());
                }
            }
            "UInt64" | "unsigned long long" | "FileSize" => {
                if let UnityValue::Integer(val) = value {
                    buffer.extend_from_slice(&(*val as u64).to_le_bytes());
                }
            }
            "float" => {
                if let UnityValue::Float(val) = value {
                    buffer.extend_from_slice(&(*val as f32).to_le_bytes());
                }
            }
            "double" => {
                if let UnityValue::Float(val) = value {
                    buffer.extend_from_slice(&val.to_le_bytes());
                }
            }
            "bool" => {
                if let UnityValue::Bool(val) = value {
                    buffer.push(if *val { 1 } else { 0 });
                    self.align_buffer(buffer, 4);
                }
            }
            "string" => {
                if let UnityValue::String(val) = value {
                    // Write string length
                    buffer.extend_from_slice(&(val.len() as u32).to_le_bytes());
                    // Write string data
                    buffer.extend_from_slice(val.as_bytes());
                    self.align_buffer(buffer, 4);
                }
            }
            _ if node.is_array() => {
                if let UnityValue::Array(elements) = value {
                    // Write array size
                    buffer.extend_from_slice(&(elements.len() as i32).to_le_bytes());

                    // Find element type
                    if let Some(array_node) = node.children.iter().find(|c| c.type_name == "Array")
                        && let Some(element_node) = array_node.children.get(1)
                    {
                        for element in elements {
                            self.serialize_value(buffer, element, element_node)?;
                        }
                    }
                }
            }
            _ => {
                // Complex object
                if let UnityValue::Object(obj) = value {
                    for child in &node.children {
                        if !child.name.is_empty()
                            && let Some(child_value) = obj.get(&child.name)
                        {
                            self.serialize_value(buffer, child_value, child)?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Align buffer to specified boundary
    fn align_buffer(&self, buffer: &mut Vec<u8>, alignment: usize) {
        let remainder = buffer.len() % alignment;
        if remainder != 0 {
            let padding = alignment - remainder;
            buffer.resize(buffer.len() + padding, 0);
        }
    }

    /// Get the TypeTree being used
    pub fn tree(&self) -> &TypeTree {
        self.tree
    }

    /// Estimate serialized size
    pub fn estimate_size(&self, data: &IndexMap<String, UnityValue>) -> usize {
        let mut size = 0;

        if let Some(root) = self.tree.nodes.first() {
            for child in &root.children {
                if !child.name.is_empty()
                    && let Some(value) = data.get(&child.name)
                {
                    size += self.estimate_value_size(value, child);
                }
            }
        }

        size
    }

    /// Estimate size of a single value
    fn estimate_value_size(&self, value: &UnityValue, node: &TypeTreeNode) -> usize {
        match node.type_name.as_str() {
            "SInt8" | "UInt8" | "char" | "bool" => 4, // Including alignment
            "SInt16" | "UInt16" | "short" | "unsigned short" => 4, // Including alignment
            "SInt32" | "UInt32" | "int" | "unsigned int" | "float" | "Type*" => 4,
            "SInt64" | "UInt64" | "long long" | "unsigned long long" | "double" | "FileSize" => 8,
            "string" => {
                if let UnityValue::String(s) = value {
                    4 + s.len() + (4 - (s.len() % 4)) % 4 // Length + data + alignment
                } else {
                    4
                }
            }
            _ if node.is_array() => {
                if let UnityValue::Array(elements) = value {
                    let mut size = 4; // Array size
                    if let Some(array_node) = node.children.iter().find(|c| c.type_name == "Array")
                        && let Some(element_node) = array_node.children.get(1)
                    {
                        for element in elements {
                            size += self.estimate_value_size(element, element_node);
                        }
                    }
                    size
                } else {
                    4
                }
            }
            _ => {
                // Complex object
                if let UnityValue::Object(obj) = value {
                    let mut size = 0;
                    for child in &node.children {
                        if !child.name.is_empty()
                            && let Some(child_value) = obj.get(&child.name)
                        {
                            size += self.estimate_value_size(child_value, child);
                        }
                    }
                    size
                } else {
                    node.byte_size.max(0) as usize
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serializer_creation() {
        let tree = TypeTree::new();
        let serializer = TypeTreeSerializer::new(&tree);
        assert!(serializer.tree().is_empty());
    }

    #[test]
    fn test_buffer_alignment() {
        let tree = TypeTree::new();
        let serializer = TypeTreeSerializer::new(&tree);

        let mut buffer = vec![1, 2, 3]; // 3 bytes
        serializer.align_buffer(&mut buffer, 4);
        assert_eq!(buffer.len(), 4); // Should be padded to 4 bytes
        assert_eq!(buffer[3], 0); // Padding should be zero
    }
}
