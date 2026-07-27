//! Unity SerializedFile model and parser.
//!
//! Parse in-memory data with [`SerializedFileParser`]. For filesystem paths,
//! use [`crate::file::load_serialized_file_with_budget`] so file access, memory mapping,
//! and cumulative load budgets stay on the crate's unified loading path.
//!
//! # Architecture
//!
//! The module is organized into several sub-modules:
//! - `header` - SerializedFile header parsing and validation
//! - `types` - Core data structures (SerializedType, FileIdentifier, etc.)
//! - `parser` - Main parsing logic for SerializedFile structures
//!
//! # Examples
//!
//! ```rust,no_run
//! use unity_asset_binary::asset::SerializedFileParser;
//!
//! // Parse a SerializedFile from in-memory data.
//! let data = std::fs::read("example.assets")?;
//! let serialized_file = SerializedFileParser::from_bytes(data)?;
//!
//! // Access objects and types
//! println!("Object count: {}", serialized_file.object_count());
//! println!("Type count: {}", serialized_file.type_count());
//!
//! // Find specific objects
//! let textures = serialized_file.objects_of_type(28); // Texture2D
//! # Ok::<(), unity_asset_binary::error::BinaryError>(())
//! ```

mod assetbundle_container;
pub mod format;
pub mod header;
mod object_type_resolver;
pub mod parser;
mod serialized_file;
pub mod types;
mod validation;

// Re-export main types for easy access
pub use format::{
    ExternalEncoding, HeaderLayout, MetadataField, MetadataPlacement, ObjectOffsetEncoding,
    ObjectTailEncoding, ObjectTypeEncoding, PathIdEncoding, SerializedFileFormat,
    SerializedFileLayout, SerializedFileRegions, TypeTreeEnablement, TypeTreeEncoding,
};
pub use header::SerializedFileHeader;
pub use parser::{SerializedFileInspection, SerializedFileParser};
pub use serialized_file::{FileStatistics, SerializedFile};
pub use types::{
    FileIdentifier, ObjectInfo, ObjectMetadata, ObjectTypeReference, SerializedType, TypeRegistry,
    class_ids,
};
