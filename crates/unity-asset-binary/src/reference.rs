//! Format-local references discovered in serialized binary object payloads.

use unity_asset_core::FieldPath;

use crate::typetree::TypeTreeTraversalStats;

/// One raw Unity `PPtr` occurrence in canonical depth-first completion order.
///
/// Null pointers and negative file IDs are retained. Resolution belongs to the workspace layer;
/// this format crate reports only the values present on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryReferenceOccurrence {
    pub field_path: FieldPath,
    pub file_id: i32,
    pub path_id: i64,
}

/// A recoverable malformed field observed during a lenient reference scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryReferenceDiagnostic {
    pub field_path: FieldPath,
    pub message: String,
}

/// Ordered references and diagnostics produced by one TypeTree traversal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BinaryReferenceScan {
    pub occurrences: Vec<BinaryReferenceOccurrence>,
    pub diagnostics: Vec<BinaryReferenceDiagnostic>,
    pub stats: TypeTreeTraversalStats,
}
