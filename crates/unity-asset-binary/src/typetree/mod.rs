//! Unity TypeTree parsing, traversal, serialization, and registry support.
//!
//! Allocation-bearing parsing is exposed by [`TypeTreeParser`] and always requires a
//! caller-owned [`unity_asset_core::AssetLoadBudget`].

pub mod builder;
mod common_strings;
pub mod parser;
pub mod registry;
pub mod serializer;
pub mod tpk;
pub mod types;

pub use builder::{TypeTreeBuilder, TypeTreeValidator, ValidationReport};
pub use parser::{
    MAX_TYPE_TREE_DEPTH, MAX_TYPE_TREE_NODES, MAX_TYPE_TREE_STRING_BUFFER, ParsingStats,
    TypeTreeParser,
};
pub use registry::{
    CompositeTypeTreeRegistry, InMemoryTypeTreeRegistry, JsonTypeTreeRegistry,
    ScriptTypeTreeGenerator, ScriptTypeTreeGeneratorRegistry, TypeTreeRegistry,
};
pub use serializer::{
    PPtrScanResult, TypeTreeParseMode, TypeTreeParseOptions, TypeTreeParseOutput,
    TypeTreeParseWarning, TypeTreeSerializer,
};
pub use tpk::TpkTypeTreeRegistry;
pub use types::{TypeInfo, TypeRegistry, TypeTree, TypeTreeNode, TypeTreeStatistics};
