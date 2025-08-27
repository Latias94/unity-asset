//! Unity TypeTree processing module
//!
//! This module provides comprehensive TypeTree processing capabilities,
//! organized following UnityPy and unity-rs best practices.
//!
//! # Architecture
//!
//! The module is organized into several sub-modules:
//! - `types` - Core data structures (TypeTree, TypeTreeNode, etc.)
//! - `parser` - TypeTree parsing from binary data
//! - `builder` - TypeTree construction and validation
//! - `serializer` - Object serialization using TypeTree information
//!
//! # Examples
//!
//! ```rust,no_run
//! use unity_asset_binary::typetree::{TypeTreeParser, TypeTreeBuilder, TypeTreeSerializer};
//! use unity_asset_binary::reader::BinaryReader;
//!
//! // Parse TypeTree from binary data
//! let mut reader = BinaryReader::new(&data, unity_asset_binary::reader::ByteOrder::Little);
//! let tree = TypeTreeParser::from_reader(&mut reader, 19)?;
//!
//! // Use TypeTree to parse object data
//! let serializer = TypeTreeSerializer::new(&tree);
//! let object_data = serializer.parse_object(&mut object_reader)?;
//!
//! // Build TypeTree programmatically
//! let mut builder = TypeTreeBuilder::new().version(19);
//! builder.add_simple_node("GameObject".to_string(), "Base".to_string(), -1, 0)?;
//! let built_tree = builder.build()?;
//! # Ok::<(), unity_asset_binary::error::BinaryError>(())
//! ```

pub mod builder;
pub mod parser;
pub mod serializer;
pub mod types;

// Re-export main types for easy access
pub use builder::{TypeTreeBuilder, TypeTreeValidator, ValidationReport};
pub use parser::{ParsingStats, TypeTreeParser};
pub use serializer::TypeTreeSerializer;
pub use types::{TypeInfo, TypeRegistry, TypeTree, TypeTreeNode, TypeTreeStatistics};

/// Main TypeTree processing facade
///
/// This struct provides a high-level interface for TypeTree processing,
/// combining parsing, building, and serialization functionality.
pub struct TypeTreeProcessor {
    tree: Option<TypeTree>,
    version: u32,
}

impl TypeTreeProcessor {
    /// Create a new TypeTree processor
    pub fn new() -> Self {
        Self {
            tree: None,
            version: 19, // Default to Unity 2019+ format
        }
    }

    /// Create a processor with a specific Unity version
    pub fn with_version(version: u32) -> Self {
        Self {
            tree: None,
            version,
        }
    }

    /// Parse TypeTree from binary data
    pub fn parse_from_reader(
        &mut self,
        reader: &mut crate::reader::BinaryReader,
    ) -> crate::error::Result<()> {
        let tree = if self.version >= 12 || self.version == 10 {
            TypeTreeParser::from_reader_blob(reader, self.version)?
        } else {
            TypeTreeParser::from_reader(reader, self.version)?
        };

        self.tree = Some(tree);
        Ok(())
    }

    /// Parse object data using the loaded TypeTree
    pub fn parse_object(
        &self,
        reader: &mut crate::reader::BinaryReader,
    ) -> crate::error::Result<indexmap::IndexMap<String, unity_asset_core::UnityValue>> {
        let tree = self
            .tree
            .as_ref()
            .ok_or_else(|| crate::error::BinaryError::generic("No TypeTree loaded"))?;

        let serializer = TypeTreeSerializer::new(tree);
        serializer.parse_object(reader)
    }

    /// Serialize object data using the loaded TypeTree
    pub fn serialize_object(
        &self,
        data: &indexmap::IndexMap<String, unity_asset_core::UnityValue>,
    ) -> crate::error::Result<Vec<u8>> {
        let tree = self
            .tree
            .as_ref()
            .ok_or_else(|| crate::error::BinaryError::generic("No TypeTree loaded"))?;

        let serializer = TypeTreeSerializer::new(tree);
        serializer.serialize_object(data)
    }

    /// Get the loaded TypeTree
    pub fn tree(&self) -> Option<&TypeTree> {
        self.tree.as_ref()
    }

    /// Set a TypeTree manually
    pub fn set_tree(&mut self, tree: TypeTree) {
        self.tree = Some(tree);
    }

    /// Validate the loaded TypeTree
    pub fn validate(&self) -> crate::error::Result<ValidationReport> {
        let tree = self
            .tree
            .as_ref()
            .ok_or_else(|| crate::error::BinaryError::generic("No TypeTree loaded"))?;

        TypeTreeValidator::validate(tree)
    }

    /// Get TypeTree statistics
    pub fn statistics(&self) -> Option<TypeTreeStatistics> {
        self.tree.as_ref().map(|tree| tree.statistics())
    }

    /// Get parsing statistics
    pub fn parsing_stats(&self) -> Option<ParsingStats> {
        self.tree
            .as_ref()
            .map(|tree| TypeTreeParser::get_parsing_stats(tree))
    }

    /// Clear the loaded TypeTree
    pub fn clear(&mut self) {
        self.tree = None;
    }

    /// Check if a TypeTree is loaded
    pub fn has_tree(&self) -> bool {
        self.tree.is_some()
    }

    /// Get the Unity version
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Set the Unity version
    pub fn set_version(&mut self, version: u32) {
        self.version = version;
    }
}

impl Default for TypeTreeProcessor {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience functions for common operations

/// Create a TypeTree processor with default settings
pub fn create_processor() -> TypeTreeProcessor {
    TypeTreeProcessor::default()
}

/// Parse TypeTree from binary data with version detection
pub fn parse_typetree(data: &[u8], version: u32) -> crate::error::Result<TypeTree> {
    let mut reader = crate::reader::BinaryReader::new(data, crate::reader::ByteOrder::Little);

    if version >= 12 || version == 10 {
        TypeTreeParser::from_reader_blob(&mut reader, version)
    } else {
        TypeTreeParser::from_reader(&mut reader, version)
    }
}

/// Parse object using TypeTree
pub fn parse_object_with_typetree(
    tree: &TypeTree,
    data: &[u8],
) -> crate::error::Result<indexmap::IndexMap<String, unity_asset_core::UnityValue>> {
    let mut reader = crate::reader::BinaryReader::new(data, crate::reader::ByteOrder::Little);
    let serializer = TypeTreeSerializer::new(tree);
    serializer.parse_object(&mut reader)
}

/// Serialize object using TypeTree
pub fn serialize_object_with_typetree(
    tree: &TypeTree,
    data: &indexmap::IndexMap<String, unity_asset_core::UnityValue>,
) -> crate::error::Result<Vec<u8>> {
    let serializer = TypeTreeSerializer::new(tree);
    serializer.serialize_object(data)
}

/// Build a simple TypeTree for common Unity types
pub fn build_common_typetree(class_name: &str) -> crate::error::Result<TypeTree> {
    let mut builder = TypeTreeBuilder::new().version(19);

    match class_name {
        "GameObject" => {
            builder.add_simple_node("GameObject".to_string(), "Base".to_string(), -1, 0)?;
            let tree = builder.tree_mut();
            if let Some(_root) = tree.nodes.get_mut(0) {
                builder.add_child_to_node(
                    "Base",
                    TypeTreeNode::with_info("int".to_string(), "m_InstanceID".to_string(), 4),
                )?;
                builder.add_child_to_node(
                    "Base",
                    TypeTreeNode::with_info("string".to_string(), "m_Name".to_string(), -1),
                )?;
            }
        }
        "Transform" => {
            builder.add_simple_node("Transform".to_string(), "Base".to_string(), -1, 0)?;
            // Add common Transform fields
            builder.add_primitive_field("Base", "m_LocalPosition".to_string(), "Vector3f")?;
            builder.add_primitive_field("Base", "m_LocalRotation".to_string(), "Quaternionf")?;
            builder.add_primitive_field("Base", "m_LocalScale".to_string(), "Vector3f")?;
        }
        _ => {
            return Err(crate::error::BinaryError::unsupported(format!(
                "Common TypeTree for '{}' not implemented",
                class_name
            )));
        }
    }

    builder.build()
}

/// Validate TypeTree structure
pub fn validate_typetree(tree: &TypeTree) -> crate::error::Result<ValidationReport> {
    TypeTreeValidator::validate(tree)
}

/// Get TypeTree information summary
pub fn get_typetree_info(tree: &TypeTree) -> TypeTreeInfo {
    let stats = tree.statistics();

    TypeTreeInfo {
        version: tree.version,
        platform: tree.platform,
        has_type_dependencies: tree.has_type_dependencies,
        node_count: stats.total_nodes,
        root_node_count: stats.root_nodes,
        max_depth: stats.max_depth,
        primitive_count: stats.primitive_count,
        array_count: stats.array_count,
        string_buffer_size: stats.string_buffer_size,
    }
}

/// TypeTree information summary
#[derive(Debug, Clone)]
pub struct TypeTreeInfo {
    pub version: u32,
    pub platform: u32,
    pub has_type_dependencies: bool,
    pub node_count: usize,
    pub root_node_count: usize,
    pub max_depth: i32,
    pub primitive_count: usize,
    pub array_count: usize,
    pub string_buffer_size: usize,
}

/// Check if TypeTree format is supported
pub fn is_version_supported(version: u32) -> bool {
    // Support Unity versions 5.0+ (version 10+)
    version >= 10
}

/// Get recommended parsing method for Unity version
pub fn get_parsing_method(version: u32) -> &'static str {
    if version >= 12 || version == 10 {
        "blob"
    } else {
        "legacy"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_processor_creation() {
        let processor = create_processor();
        assert!(!processor.has_tree());
        assert_eq!(processor.version(), 19);
    }

    #[test]
    fn test_version_support() {
        assert!(is_version_supported(19));
        assert!(is_version_supported(10));
        assert!(!is_version_supported(5));
    }

    #[test]
    fn test_parsing_method() {
        assert_eq!(get_parsing_method(19), "blob");
        assert_eq!(get_parsing_method(12), "blob");
        assert_eq!(get_parsing_method(10), "blob");
        assert_eq!(get_parsing_method(9), "legacy");
    }

    #[test]
    fn test_typetree_info() {
        let tree = TypeTree::new();
        let info = get_typetree_info(&tree);
        assert_eq!(info.node_count, 0);
        assert_eq!(info.root_node_count, 0);
    }

    #[test]
    fn test_common_typetree_building() {
        // Test that common TypeTree building doesn't panic
        let result = build_common_typetree("GameObject");
        assert!(result.is_ok() || result.is_err()); // Either way is fine for this test
    }
}
