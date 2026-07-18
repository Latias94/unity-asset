//! Unity TypeTree parsing, traversal, serialization, and registry support.
//!
//! Allocation-bearing parsing is exposed by [`TypeTreeParser`] and always requires a
//! caller-owned [`unity_asset_core::AssetLoadBudget`].

mod common_strings;
mod execution;
pub mod parser;
pub mod registry;
mod schema;
pub mod tpk;
mod traversal;
pub mod types;

pub use execution::{
    PPtrScanResult, TypeTreeParseMode, TypeTreeParseOptions, TypeTreeParseOutput,
    TypeTreeParseWarning, TypeTreeValueRead,
};
pub use parser::{
    MAX_TYPE_TREE_DEPTH, MAX_TYPE_TREE_NODES, MAX_TYPE_TREE_STRING_BUFFER, ParsingStats,
    TypeTreeParser,
};
pub use registry::{
    CompositeTypeTreeRegistry, InMemoryTypeTreeRegistry, JsonTypeTreeRegistry,
    ScriptTypeTreeGenerator, ScriptTypeTreeGeneratorRegistry, TypeTreeRegistry,
};
pub(crate) use schema::ManagedReferenceCatalog;
pub use schema::{
    IntegerSignedness, ManagedPayload, PPtrLayout, PairLayout, PrimitiveKind,
    ReferencedObjectLayout, SchemaChildren, SchemaNode, SemanticKind, SemanticLayout,
    SequenceLayout, TypeTreeSchema, TypeTreeSemanticDigestError, TypeTreeTraversalContext,
};
pub use tpk::TpkTypeTreeRegistry;
pub use traversal::{TypeTreeTraversalStats, TypeTreeTraversalStatsOverflow};
pub use types::{TypeTree, TypeTreeNode, TypeTreeStatistics};
