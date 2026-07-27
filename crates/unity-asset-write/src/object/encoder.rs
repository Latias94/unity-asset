//! Atomic, schema-aware encoding for one SerializedFile object.

use std::collections::TryReserveError;
use std::mem::{replace, size_of};
use std::sync::Arc;

use indexmap::IndexMap;
use thiserror::Error;
use unity_asset_binary::asset::SerializedFile;
use unity_asset_binary::error::BinaryError;
use unity_asset_binary::object::ObjectHandle;
use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{
    PrimitiveKind, SchemaNode, SemanticKind, SemanticLayout, TypeTreeParseMode,
    TypeTreeParseOptions, TypeTreeSchema, TypeTreeSemanticDigestError, TypeTreeTraversalContext,
    TypeTreeTraversalStats, TypeTreeWriteError,
};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, DigestV1, FieldPath, FieldPathSegment, SemanticDigestError,
    UnityValue, ValuePathError, arc_value_allocation_bytes, field_schema_digest,
    semantic_value_digest,
};

/// Writer-facing alias for the shared stable [`UnityValue`] shape discriminator.
pub use unity_asset_core::UnityValueKind as SerializedValueKind;

/// Stable, read-only field projection for one compiled binary `PPtr`.
///
/// The projection exposes only the information needed to synthesize the two wire integers. It
/// deliberately does not expose the candidate's backing [`TypeTreeSchema`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerializedPPtrLayout<'schema> {
    file_field: &'schema str,
    file_index: usize,
    file_primitive: PrimitiveKind,
    path_field: &'schema str,
    path_index: usize,
    path_primitive: PrimitiveKind,
    field_count: usize,
}

impl<'schema> SerializedPPtrLayout<'schema> {
    #[must_use]
    pub const fn file_field(self) -> &'schema str {
        self.file_field
    }

    #[must_use]
    pub const fn file_index(self) -> usize {
        self.file_index
    }

    #[must_use]
    pub const fn file_primitive(self) -> PrimitiveKind {
        self.file_primitive
    }

    #[must_use]
    pub const fn path_field(self) -> &'schema str {
        self.path_field
    }

    #[must_use]
    pub const fn path_index(self) -> usize {
        self.path_index
    }

    #[must_use]
    pub const fn path_primitive(self) -> PrimitiveKind {
        self.path_primitive
    }

    #[must_use]
    pub const fn field_count(self) -> usize {
        self.field_count
    }
}

/// Stable field names for one compiled binary managed-reference object.
///
/// The projection lets semantic mutation lowering identify the replacement value's runtime type
/// without exposing TypeTree nodes or the managed-reference catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerializedManagedReferenceLayout<'schema> {
    type_field: &'schema str,
    class_field: &'schema str,
    namespace_field: &'schema str,
    assembly_field: &'schema str,
    payload_field: &'schema str,
}

impl<'schema> SerializedManagedReferenceLayout<'schema> {
    #[must_use]
    pub const fn type_field(self) -> &'schema str {
        self.type_field
    }

    #[must_use]
    pub const fn class_field(self) -> &'schema str {
        self.class_field
    }

    #[must_use]
    pub const fn namespace_field(self) -> &'schema str {
        self.namespace_field
    }

    #[must_use]
    pub const fn assembly_field(self) -> &'schema str {
        self.assembly_field
    }

    #[must_use]
    pub const fn payload_field(self) -> &'schema str {
        self.payload_field
    }
}

/// Borrowed runtime type discriminator for a binary managed-reference replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerializedManagedReferenceType<'value> {
    class_name: &'value str,
    namespace: &'value str,
    assembly_name: &'value str,
}

impl<'value> SerializedManagedReferenceType<'value> {
    #[must_use]
    pub const fn new(
        class_name: &'value str,
        namespace: &'value str,
        assembly_name: &'value str,
    ) -> Self {
        Self {
            class_name,
            namespace,
            assembly_name,
        }
    }

    #[must_use]
    pub const fn class_name(self) -> &'value str {
        self.class_name
    }

    #[must_use]
    pub const fn namespace(self) -> &'value str {
        self.namespace
    }

    #[must_use]
    pub const fn assembly_name(self) -> &'value str {
        self.assembly_name
    }
}

/// Opaque schema location used while lowering a semantic replacement.
///
/// Callers can descend named fields and collection elements, and can inspect a terminal PPtr, but
/// cannot retain or execute the underlying TypeTree program.
#[derive(Clone, Copy)]
pub struct SerializedValueSchema<'schema> {
    schema: &'schema TypeTreeSchema,
    node: SchemaNode<'schema>,
    context: TypeTreeTraversalContext,
}

impl std::fmt::Debug for SerializedValueSchema<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SerializedValueSchema")
            .field("name", &self.node.name())
            .field("kind", &self.node.kind())
            .finish_non_exhaustive()
    }
}

impl<'schema> SerializedValueSchema<'schema> {
    #[must_use]
    pub fn kind(self) -> SemanticKind {
        self.node.kind()
    }

    /// Resolves one direct named schema field without consulting replacement values.
    #[must_use]
    pub fn field(self, name: &str) -> Option<Self> {
        let mut context = self.context;
        for child in self.node.children() {
            let Some(child_context) = context.descend(self.node, child) else {
                continue;
            };
            if child.name() == name {
                return Some(Self {
                    schema: self.schema,
                    node: child,
                    context: child_context,
                });
            }
        }
        None
    }

    /// Resolves one named field against a current staged value.
    ///
    /// Unlike [`Self::field`], this follows a managed-reference payload's runtime type identity.
    #[must_use]
    pub fn field_for_value(self, name: &str, current: &UnityValue) -> Option<Self> {
        let UnityValue::Object(fields) = current else {
            return None;
        };
        let (node, context) =
            resolve_named_schema_child(self.schema, self.node, self.context, fields, name)?;
        Some(Self {
            schema: self.schema,
            node,
            context,
        })
    }

    /// Returns the stable discriminator and payload field names for a managed-reference object.
    #[must_use]
    pub fn managed_reference_layout(self) -> Option<SerializedManagedReferenceLayout<'schema>> {
        let SemanticLayout::ReferencedObject(layout) = self.node.semantic_layout() else {
            return None;
        };
        Some(SerializedManagedReferenceLayout {
            type_field: layout.type_node().name(),
            class_field: layout.class_field().name(),
            namespace_field: layout.namespace_field().name(),
            assembly_field: layout.assembly_field().name(),
            payload_field: layout.payload().node().name(),
        })
    }

    /// Resolves a direct field using the replacement value's managed runtime type.
    ///
    /// This must be used for managed payloads being inserted or whose discriminator changes;
    /// consulting the current staged value would select the previous runtime schema.
    #[must_use]
    pub fn field_for_managed_type(
        self,
        name: &str,
        runtime_type: SerializedManagedReferenceType<'_>,
    ) -> Option<Self> {
        let (node, context) = resolve_named_schema_child_for_managed_type(
            self.schema,
            self.node,
            self.context,
            name,
            runtime_type,
        )?;
        Some(Self {
            schema: self.schema,
            node,
            context,
        })
    }

    /// Resolves one array element schema. Sequence indices share one schema; pair indices select
    /// their corresponding child.
    #[must_use]
    pub fn element(self, index: usize) -> Option<Self> {
        let node = match self.node.semantic_layout() {
            SemanticLayout::Sequence(layout) | SemanticLayout::Map(layout) => layout.element(),
            SemanticLayout::Pair(layout) => match index {
                0 => layout.first(),
                1 => layout.second(),
                _ => return None,
            },
            _ => return None,
        };
        Some(Self {
            schema: self.schema,
            node,
            context: self.context,
        })
    }

    /// Returns stable PPtr field locations when this schema node is a compiled PPtr.
    #[must_use]
    pub fn pptr_layout(self) -> Option<SerializedPPtrLayout<'schema>> {
        let layout = self.node.pptr_layout()?;
        let mut file_index = None;
        let mut path_index = None;
        for (index, child) in self.node.children().enumerate() {
            if child == layout.file_child() {
                file_index = Some(index);
            }
            if child == layout.path_child() {
                path_index = Some(index);
            }
        }
        Some(SerializedPPtrLayout {
            file_field: layout.file_child().name(),
            file_index: file_index?,
            file_primitive: layout.file_primitive(),
            path_field: layout.path_child().name(),
            path_index: path_index?,
            path_primitive: layout.path_primitive(),
            field_count: self.node.child_count(),
        })
    }
}

/// Allocation-free failure from resolving the schema corresponding to a staged value path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SerializedValueSchemaError {
    #[error("field path segment count overflowed")]
    SegmentOverflow,
    #[error("staged value is missing field path segment {segment}")]
    Missing { segment: u32 },
    #[error("compiled schema cannot resolve field path segment {segment}")]
    SchemaMismatch { segment: u32 },
    #[error("field path segment {segment} expected {expected:?}, found {actual:?}")]
    TypeMismatch {
        segment: u32,
        expected: SerializedValueKind,
        actual: SerializedValueKind,
    },
    #[error("field path segment {segment} index {index} is outside sequence length {length}")]
    IndexOutOfBounds {
        segment: u32,
        index: u32,
        length: usize,
    },
}

use crate::Endian;
use crate::typetree::{TemplateRewriteStats, rewrite_object, validate_value};

const MAX_TYPETREE_SEQUENCE_LENGTH: usize = i32::MAX as usize;

/// Guard for replacing one field under a compiled TypeTree schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerializedFieldGuard {
    schema_digest: DigestV1,
    value_digest: DigestV1,
}

impl SerializedFieldGuard {
    #[must_use]
    pub const fn new(schema_digest: DigestV1, value_digest: DigestV1) -> Self {
        Self {
            schema_digest,
            value_digest,
        }
    }

    /// Builds a guard from the schema digest and currently observed field value.
    pub fn from_observed(
        object_schema_digest: DigestV1,
        path: &FieldPath,
        value: &UnityValue,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SerializedObjectEncodeError> {
        Ok(Self {
            schema_digest: field_schema_digest(object_schema_digest, path)
                .map_err(map_guard_digest_error)?,
            value_digest: semantic_value_digest(value, budget).map_err(map_guard_digest_error)?,
        })
    }

    #[must_use]
    pub const fn schema_digest(self) -> DigestV1 {
        self.schema_digest
    }

    #[must_use]
    pub const fn value_digest(self) -> DigestV1 {
        self.value_digest
    }
}

/// Guard for replacing an object's complete schema-bound semantic value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerializedObjectGuard {
    schema_digest: DigestV1,
    value_digest: DigestV1,
}

impl SerializedObjectGuard {
    #[must_use]
    pub const fn new(schema_digest: DigestV1, value_digest: DigestV1) -> Self {
        Self {
            schema_digest,
            value_digest,
        }
    }

    /// Builds a guard from the schema digest and currently observed object value.
    pub fn from_observed(
        schema_digest: DigestV1,
        value: &UnityValue,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SerializedObjectEncodeError> {
        Ok(Self {
            schema_digest,
            value_digest: semantic_value_digest(value, budget).map_err(map_guard_digest_error)?,
        })
    }

    #[must_use]
    pub const fn schema_digest(self) -> DigestV1 {
        self.schema_digest
    }

    #[must_use]
    pub const fn value_digest(self) -> DigestV1 {
        self.value_digest
    }
}

/// One closed sequence edit applied after its collection guard succeeds.
#[derive(Debug, PartialEq)]
pub enum SerializedSequenceEdit {
    Insert { index: u32, value: UnityValue },
    Replace { index: u32, value: UnityValue },
    Remove { index: u32 },
    Move { from: u32, to: u32 },
    Clear,
}

/// One ordered semantic mutation for a SerializedFile object.
#[derive(Debug, PartialEq)]
pub struct SerializedObjectMutation {
    ordinal: u32,
    kind: SerializedObjectMutationKind,
}

#[derive(Debug, PartialEq)]
enum SerializedObjectMutationKind {
    ReplaceField {
        path: FieldPath,
        guard: SerializedFieldGuard,
        replacement: UnityValue,
    },
    ReplaceObject {
        guard: SerializedObjectGuard,
        replacement: IndexMap<String, UnityValue>,
    },
    EditSequence {
        path: FieldPath,
        guard: SerializedFieldGuard,
        edit: SerializedSequenceEdit,
    },
}

impl SerializedObjectMutation {
    #[must_use]
    pub fn replace_field(
        ordinal: u32,
        path: FieldPath,
        guard: SerializedFieldGuard,
        replacement: UnityValue,
    ) -> Self {
        Self {
            ordinal,
            kind: SerializedObjectMutationKind::ReplaceField {
                path,
                guard,
                replacement,
            },
        }
    }

    #[must_use]
    pub fn replace_object(
        ordinal: u32,
        guard: SerializedObjectGuard,
        replacement: IndexMap<String, UnityValue>,
    ) -> Self {
        Self {
            ordinal,
            kind: SerializedObjectMutationKind::ReplaceObject { guard, replacement },
        }
    }

    #[must_use]
    pub fn edit_sequence(
        ordinal: u32,
        path: FieldPath,
        guard: SerializedFieldGuard,
        edit: SerializedSequenceEdit,
    ) -> Self {
        Self {
            ordinal,
            kind: SerializedObjectMutationKind::EditSequence { path, guard, edit },
        }
    }

    #[must_use]
    pub const fn ordinal(&self) -> u32 {
        self.ordinal
    }
}

/// Explicit acknowledgement required by the raw replacement escape hatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsafeRawObjectAcknowledgement {
    WireInvariantsAreCallersResponsibilityV1,
}

/// A digest-guarded raw replacement that bypasses TypeTree semantics.
#[derive(Debug)]
pub struct UnsafeRawObjectReplacement {
    expected_raw_digest: DigestV1,
    bytes: Vec<u8>,
    acknowledgement: UnsafeRawObjectAcknowledgement,
}

impl UnsafeRawObjectReplacement {
    #[must_use]
    pub fn new(
        expected_raw_digest: DigestV1,
        bytes: Vec<u8>,
        acknowledgement: UnsafeRawObjectAcknowledgement,
    ) -> Self {
        Self {
            expected_raw_digest,
            bytes,
            acknowledgement,
        }
    }
}

/// Encoding path used to produce an object override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerializedObjectEncodingMode {
    Semantic,
    UnsafeRaw,
}

/// Pass counts and canonical TypeTree traversal measurements for one encoding.
///
/// Validation uses the canonical writer traversal but retains no wire buffer. Every candidate
/// validation starts from virtual offset zero; `validation.wire_bytes` is the sum of those
/// standalone checked extents, not an object byte range or bytes written. Alignment therefore
/// describes each candidate in isolation. `validation.owned_bytes` remains zero, and unnamed
/// fields preserved from the template are excluded from the virtual extent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SerializedObjectEncodingStats {
    pub parse_passes: u32,
    pub validation_passes: u32,
    pub rewrite_passes: u32,
    pub operations_applied: u64,
    pub parse: TypeTreeTraversalStats,
    /// Aggregate zero-origin virtual extents from operation-level validation passes.
    pub validation: TypeTreeTraversalStats,
    pub rewrite_input: TypeTreeTraversalStats,
    pub rewrite_output: TypeTreeTraversalStats,
    pub preserved_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct OperationValidationStats {
    passes: u32,
    traversal: TypeTreeTraversalStats,
}

impl OperationValidationStats {
    fn record(
        &mut self,
        traversal: TypeTreeTraversalStats,
    ) -> Result<(), SerializedObjectEncodeError> {
        self.passes =
            self.passes
                .checked_add(1)
                .ok_or(SerializedObjectEncodeError::ArithmeticOverflow {
                    resource: "TypeTree validation pass count",
                })?;
        self.traversal = self.traversal.checked_add(traversal).map_err(|_| {
            SerializedObjectEncodeError::ArithmeticOverflow {
                resource: "TypeTree validation statistics",
            }
        })?;
        Ok(())
    }
}

/// Immutable bytes produced only after every requested operation succeeds.
#[derive(Debug)]
#[must_use = "apply the encoded bytes to a SerializedFile edit or artifact"]
pub struct EncodedSerializedObject {
    path_id: i64,
    class_id: i32,
    mode: SerializedObjectEncodingMode,
    schema_digest: Option<DigestV1>,
    original_digest: DigestV1,
    output_digest: DigestV1,
    bytes: Vec<u8>,
    semantic_value: Option<UnityValue>,
    stats: SerializedObjectEncodingStats,
}

impl EncodedSerializedObject {
    #[must_use]
    pub const fn path_id(&self) -> i64 {
        self.path_id
    }

    #[must_use]
    pub const fn class_id(&self) -> i32 {
        self.class_id
    }

    #[must_use]
    pub const fn mode(&self) -> SerializedObjectEncodingMode {
        self.mode
    }

    #[must_use]
    pub const fn schema_digest(&self) -> Option<DigestV1> {
        self.schema_digest
    }

    #[must_use]
    pub const fn original_digest(&self) -> DigestV1 {
        self.original_digest
    }

    #[must_use]
    pub const fn output_digest(&self) -> DigestV1 {
        self.output_digest
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the fully validated staged value for semantic encodings.
    ///
    /// Unsafe raw replacements deliberately return `None`: their acknowledgement proves only
    /// caller ownership of wire invariants and does not invent semantic state for a prepared view.
    #[must_use]
    pub const fn semantic_value(&self) -> Option<&UnityValue> {
        self.semantic_value.as_ref()
    }

    #[must_use]
    pub const fn stats(&self) -> SerializedObjectEncodingStats {
        self.stats
    }

    pub(crate) fn into_file_edit_parts(self) -> (i64, i32, DigestV1, Vec<u8>) {
        (
            self.path_id,
            self.class_id,
            self.original_digest,
            self.bytes,
        )
    }
}

/// Failure from atomic single-object encoding.
#[derive(Debug, Error)]
pub enum SerializedObjectEncodeError {
    #[error("SerializedFile object {path_id} was not found")]
    ObjectNotFound { path_id: i64 },
    #[error("TypeTree is unavailable for object {path_id} (class {class_id})")]
    TypeTreeUnavailable { path_id: i64, class_id: i32 },
    #[error("failed to select TypeTree schema for object {path_id}: {source}")]
    Schema {
        path_id: i64,
        #[source]
        source: BinaryError,
    },
    #[error("failed to compute TypeTree schema digest for object {path_id}: {source}")]
    SchemaDigest {
        path_id: i64,
        #[source]
        source: TypeTreeSemanticDigestError,
    },
    #[error("failed to read raw bytes for object {path_id}: {source}")]
    ReadRaw {
        path_id: i64,
        #[source]
        source: BinaryError,
    },
    #[error("failed to parse object {path_id} through its TypeTree: {source}")]
    Parse {
        path_id: i64,
        #[source]
        source: BinaryError,
    },
    #[error("the complete TypeTree parse for object {path_id} stopped early")]
    IncompleteParse { path_id: i64 },
    #[error("semantic object encoding requires at least one operation")]
    NoOperations,
    #[error("operation ordinals must increase: {current} follows {previous}")]
    OperationOrder { previous: u32, current: u32 },
    #[error("operation {ordinal} uses a stale field guard for object {path_id}")]
    StaleFieldGuard { path_id: i64, ordinal: u32 },
    #[error("semantic operation count overflow")]
    OperationCountOverflow,
    #[error("operation {ordinal} must use object replacement for the root path")]
    RootFieldReplacement { ordinal: u32 },
    #[error("semantic operations produced a non-object root: {actual:?}")]
    RootTypeInvariant { actual: SerializedValueKind },
    #[error(
        "operation {ordinal} replacement at {path} for object {path_id} has {actual_fields} named fields; the TypeTree expects {expected_fields}"
    )]
    ReplacementShape {
        path_id: i64,
        ordinal: u32,
        path: FieldPath,
        expected_fields: usize,
        actual_fields: usize,
    },
    #[error(
        "operation {ordinal} replacement at {path} is not representable by object {path_id}'s TypeTree: {source}"
    )]
    ReplacementValue {
        path_id: i64,
        ordinal: u32,
        path: FieldPath,
        #[source]
        source: TypeTreeWriteError,
    },
    #[error("operation {ordinal} cannot resolve field path {path} at segment {segment}")]
    PathMissing {
        ordinal: u32,
        path: FieldPath,
        segment: u32,
    },
    #[error(
        "operation {ordinal} path {path} at segment {segment} cannot be resolved in the compiled TypeTree"
    )]
    PathSchemaMismatch {
        ordinal: u32,
        path: FieldPath,
        segment: u32,
    },
    #[error(
        "operation {ordinal} expected {expected:?} at {path} segment {segment}, found {actual:?}"
    )]
    PathTypeMismatch {
        ordinal: u32,
        path: FieldPath,
        segment: u32,
        expected: SerializedValueKind,
        actual: SerializedValueKind,
    },
    #[error(
        "operation {ordinal} index {index} is outside sequence length {length} at {path} segment {segment}"
    )]
    PathIndexOutOfBounds {
        ordinal: u32,
        path: FieldPath,
        segment: u32,
        index: u32,
        length: usize,
    },
    #[error("operation {ordinal} field-schema guard failed at {path}")]
    FieldSchemaGuardMismatch {
        ordinal: u32,
        path: FieldPath,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error("operation {ordinal} field-value guard failed at {path}")]
    FieldValueGuardMismatch {
        ordinal: u32,
        path: FieldPath,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error("operation {ordinal} object-schema guard failed")]
    ObjectSchemaGuardMismatch {
        ordinal: u32,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error("operation {ordinal} object-value guard failed")]
    ObjectValueGuardMismatch {
        ordinal: u32,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error("operation {ordinal} expected a sequence at {path}, found {actual:?}")]
    SequenceTypeMismatch {
        ordinal: u32,
        path: FieldPath,
        actual: SerializedValueKind,
    },
    #[error("operation {ordinal} sequence index {index} is outside length {length} at {path}")]
    SequenceIndexOutOfBounds {
        ordinal: u32,
        path: FieldPath,
        index: u32,
        length: usize,
    },
    #[error(
        "operation {ordinal} would grow the sequence at {path} to {length} elements; TypeTree lengths are limited to {maximum}"
    )]
    SequenceLengthOverflow {
        ordinal: u32,
        path: FieldPath,
        length: usize,
        maximum: usize,
    },
    #[error("operation {ordinal} sequence move keeps index {index} unchanged at {path}")]
    NoopSequenceMove {
        ordinal: u32,
        path: FieldPath,
        index: u32,
    },
    #[error("failed to reserve {requested} elements for {resource}: {source}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error("arithmetic overflow while accounting for {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("semantic guard digest failed: {source}")]
    GuardDigest {
        #[source]
        source: SemanticDigestError,
    },
    #[error("failed to rewrite object {path_id}: {source}")]
    Rewrite {
        path_id: i64,
        #[source]
        source: TypeTreeWriteError,
    },
    #[error("raw replacement digest guard failed for object {path_id}")]
    RawDigestMismatch {
        path_id: i64,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error(transparent)]
    Budget(#[from] BudgetError),
}

/// Encoder bound to one immutable SerializedFile object.
#[derive(Debug, Clone, Copy)]
pub struct SerializedObjectEncoder<'file> {
    handle: ObjectHandle<'file>,
}

/// A raw object whose original byte digest has already passed its caller guard.
#[derive(Debug)]
#[must_use = "finish the prepared raw object to produce encoded bytes"]
pub struct PreparedUnsafeRawObject<'file> {
    handle: ObjectHandle<'file>,
    original_digest: DigestV1,
}

/// One parsed SerializedFile object receiving guarded mutations in plan order.
///
/// The candidate retains semantic state between calls to [`Self::apply`]. This lets a workspace
/// interleave operations for several objects without grouping them ahead of validation, while the
/// final object is still rewritten exactly once by [`Self::finish`].
#[derive(Debug)]
#[must_use = "finish the candidate to produce encoded object bytes"]
pub struct SerializedObjectCandidate<'file> {
    handle: ObjectHandle<'file>,
    lineage: Arc<CandidateLineage>,
    schema: TypeTreeSchema,
    schema_digest: DigestV1,
    original: &'file [u8],
    original_digest: DigestV1,
    root: UnityValue,
    endian: Endian,
    parse_stats: TypeTreeTraversalStats,
    previous_ordinal: Option<u32>,
    operation_count: u64,
    validation_stats: OperationValidationStats,
}

#[derive(Debug)]
struct CandidateLineage;

/// A field guard bound to one unchanged serialized-object candidate generation.
#[derive(Debug)]
#[must_use = "prepare or discard the validated field guard"]
pub struct ValidatedSerializedFieldGuard<'file> {
    handle: ObjectHandle<'file>,
    lineage: Arc<CandidateLineage>,
    schema_digest: DigestV1,
    previous_ordinal: Option<u32>,
    operation_count: u64,
    next_operation_count: u64,
    ordinal: u32,
    path: FieldPath,
}

impl ValidatedSerializedFieldGuard<'_> {
    #[must_use]
    pub const fn path(&self) -> &FieldPath {
        &self.path
    }
}

/// A fully validated serialized field replacement awaiting an infallible candidate commit.
#[derive(Debug)]
#[must_use = "commit the prepared field replacement after dependent allocations succeed"]
pub struct PreparedSerializedFieldReplace<'candidate> {
    target: &'candidate mut UnityValue,
    previous_ordinal: &'candidate mut Option<u32>,
    validation_stats: &'candidate mut OperationValidationStats,
    operation_count: &'candidate mut u64,
    replacement: UnityValue,
    ordinal: u32,
    next_validation_stats: OperationValidationStats,
    next_operation_count: u64,
}

impl PreparedSerializedFieldReplace<'_> {
    /// Installs the already validated replacement without allocation or further TypeTree work.
    pub fn commit(self) {
        *self.target = self.replacement;
        *self.previous_ordinal = Some(self.ordinal);
        *self.validation_stats = self.next_validation_stats;
        *self.operation_count = self.next_operation_count;
    }
}

impl<'file> SerializedObjectEncoder<'file> {
    pub fn new(
        file: &'file SerializedFile,
        path_id: i64,
    ) -> Result<Self, SerializedObjectEncodeError> {
        let handle = file
            .find_object_handle(path_id)
            .ok_or(SerializedObjectEncodeError::ObjectNotFound { path_id })?;
        Ok(Self { handle })
    }

    #[must_use]
    pub fn path_id(&self) -> i64 {
        self.handle.path_id()
    }

    #[must_use]
    pub fn class_id(&self) -> i32 {
        self.handle.class_id()
    }

    /// Parses the original object once and opens an ordered semantic candidate.
    pub fn begin_semantic(
        self,
        budget: &mut AssetLoadBudget,
    ) -> Result<SerializedObjectCandidate<'file>, SerializedObjectEncodeError> {
        let path_id = self.path_id();
        let class_id = self.class_id();
        let schema = self
            .handle
            .schema(budget)
            .map_err(|error| map_binary_error(path_id, BinaryStage::Schema, error))?
            .ok_or(SerializedObjectEncodeError::TypeTreeUnavailable { path_id, class_id })?;
        let schema_digest = schema
            .semantic_digest_with_budget(budget)
            .map_err(|error| map_schema_digest_error(path_id, error))?;
        let original = self
            .handle
            .raw_data()
            .map_err(|source| SerializedObjectEncodeError::ReadRaw { path_id, source })?;
        let original_digest = DigestV1::hash_bytes(original);
        let mut reader = BinaryReader::new(original, self.handle.file().header.byte_order());
        let parsed = schema
            .read_object(
                &mut reader,
                budget,
                TypeTreeParseOptions {
                    mode: TypeTreeParseMode::Strict,
                },
            )
            .map_err(|error| map_binary_error(path_id, BinaryStage::Parse, error))?;
        if !parsed.complete {
            return Err(SerializedObjectEncodeError::IncompleteParse { path_id });
        }

        let lineage_bytes = arc_value_allocation_bytes::<CandidateLineage>().map_err(|_| {
            SerializedObjectEncodeError::ArithmeticOverflow {
                resource: "serialized candidate lineage",
            }
        })?;
        budget.consume_bytes(lineage_bytes)?;
        Ok(SerializedObjectCandidate {
            handle: self.handle,
            lineage: Arc::new(CandidateLineage),
            schema,
            schema_digest,
            original,
            original_digest,
            root: UnityValue::Object(parsed.properties),
            endian: endian_for(self.handle.file().header.byte_order()),
            parse_stats: parsed.stats,
            previous_ordinal: None,
            operation_count: 0,
            validation_stats: OperationValidationStats::default(),
        })
    }

    /// Applies all operations in ordinal order and rewrites the object exactly once.
    pub fn encode_semantic(
        self,
        operations: impl IntoIterator<Item = SerializedObjectMutation>,
        budget: &mut AssetLoadBudget,
    ) -> Result<EncodedSerializedObject, SerializedObjectEncodeError> {
        let mut operations = operations.into_iter();
        let first = operations
            .next()
            .ok_or(SerializedObjectEncodeError::NoOperations)?;
        let mut candidate = self.begin_semantic(budget)?;
        for operation in std::iter::once(first).chain(operations) {
            candidate.apply(operation, budget)?;
        }
        candidate.finish(budget)
    }

    /// Replaces raw bytes only after verifying the original digest and explicit acknowledgement.
    pub fn encode_unsafe_raw(
        self,
        replacement: UnsafeRawObjectReplacement,
        budget: &mut AssetLoadBudget,
    ) -> Result<EncodedSerializedObject, SerializedObjectEncodeError> {
        let UnsafeRawObjectReplacement {
            expected_raw_digest,
            bytes,
            acknowledgement,
        } = replacement;
        self.prepare_unsafe_raw(expected_raw_digest)?
            .finish(bytes, acknowledgement, budget)
    }

    /// Verifies the original raw bytes now and returns an opaque completion token.
    ///
    /// This separates operation-ordered guard validation from later artifact encoding without
    /// hashing the immutable source object twice.
    pub fn prepare_unsafe_raw(
        self,
        expected_raw_digest: DigestV1,
    ) -> Result<PreparedUnsafeRawObject<'file>, SerializedObjectEncodeError> {
        let path_id = self.path_id();
        let original = self
            .handle
            .raw_data()
            .map_err(|source| SerializedObjectEncodeError::ReadRaw { path_id, source })?;
        let actual = DigestV1::hash_bytes(original);
        if expected_raw_digest != actual {
            return Err(SerializedObjectEncodeError::RawDigestMismatch {
                path_id,
                expected: expected_raw_digest,
                actual,
            });
        }

        Ok(PreparedUnsafeRawObject {
            handle: self.handle,
            original_digest: actual,
        })
    }
}

impl PreparedUnsafeRawObject<'_> {
    /// Completes a previously guarded raw replacement without rereading the source object.
    pub fn finish(
        self,
        bytes: Vec<u8>,
        acknowledgement: UnsafeRawObjectAcknowledgement,
        budget: &mut AssetLoadBudget,
    ) -> Result<EncodedSerializedObject, SerializedObjectEncodeError> {
        let UnsafeRawObjectAcknowledgement::WireInvariantsAreCallersResponsibilityV1 =
            acknowledgement;
        let path_id = self.handle.path_id();
        let class_id = self.handle.class_id();
        let byte_count = usize_to_u64(bytes.len(), "raw replacement bytes")?;
        budget.check_bytes(byte_count)?;
        budget.check_entries(1)?;
        budget.consume_bytes(byte_count)?;
        budget.consume_entries(1)?;
        let output_digest = DigestV1::hash_bytes(&bytes);

        Ok(EncodedSerializedObject {
            path_id,
            class_id,
            mode: SerializedObjectEncodingMode::UnsafeRaw,
            schema_digest: None,
            original_digest: self.original_digest,
            output_digest,
            bytes,
            semantic_value: None,
            stats: SerializedObjectEncodingStats {
                operations_applied: 1,
                ..SerializedObjectEncodingStats::default()
            },
        })
    }
}

impl<'file> SerializedObjectCandidate<'file> {
    #[must_use]
    pub fn path_id(&self) -> i64 {
        self.handle.path_id()
    }

    #[must_use]
    pub fn class_id(&self) -> i32 {
        self.handle.class_id()
    }

    #[must_use]
    pub const fn schema_digest(&self) -> DigestV1 {
        self.schema_digest
    }

    /// Returns the current staged root value without granting mutable access.
    #[must_use]
    pub const fn semantic_value(&self) -> &UnityValue {
        &self.root
    }

    /// Returns an opaque projection of the root schema for recursive replacement lowering.
    #[must_use]
    pub fn root_value_schema(&self) -> SerializedValueSchema<'_> {
        SerializedValueSchema {
            schema: &self.schema,
            node: self.schema.root(),
            context: TypeTreeTraversalContext::root(),
        }
    }

    /// Resolves the schema corresponding to the candidate's current staged value at `path`.
    ///
    /// Resolution observes earlier successful operations on this candidate. The returned value is
    /// a read-only location projection and cannot mutate or execute the backing TypeTree.
    pub fn value_schema_at_path(
        &self,
        path: &FieldPath,
    ) -> Result<SerializedValueSchema<'_>, SerializedValueSchemaError> {
        let location = schema_location_at_path(&self.schema, &self.root, path)
            .map_err(SerializedValueSchemaError::from)?
            .target;
        Ok(SerializedValueSchema {
            schema: &self.schema,
            node: location.node,
            context: location.context,
        })
    }

    /// Returns the current staged semantic value at a field path without allocating.
    pub fn value_at_path(&self, path: &FieldPath) -> Result<&UnityValue, ValuePathError> {
        self.root.value_at_path(path)
    }

    /// Completes every fallible step of a field replacement without changing the candidate.
    ///
    /// Dropping the returned token leaves the semantic root and operation cursors unchanged.
    pub fn prepare_replace_field(
        &mut self,
        ordinal: u32,
        path: FieldPath,
        guard: SerializedFieldGuard,
        replacement: UnityValue,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedSerializedFieldReplace<'_>, SerializedObjectEncodeError> {
        let validated = self.validate_replace_field_guard(ordinal, path, guard, budget)?;
        self.prepare_validated_replace_field(validated, replacement, budget)
    }

    /// Verifies an ordered field guard without borrowing or changing the candidate.
    ///
    /// The token retains the immutable object handle and the candidate's operation generation, so
    /// it cannot be replayed against another object or after an intervening mutation.
    pub fn validate_replace_field_guard(
        &self,
        ordinal: u32,
        path: FieldPath,
        guard: SerializedFieldGuard,
        budget: &mut AssetLoadBudget,
    ) -> Result<ValidatedSerializedFieldGuard<'file>, SerializedObjectEncodeError> {
        let next_operation_count = self
            .operation_count
            .checked_add(1)
            .ok_or(SerializedObjectEncodeError::OperationCountOverflow)?;
        let path = validate_field_replacement_guard(
            &self.schema,
            self.schema_digest,
            &self.root,
            self.previous_ordinal,
            ordinal,
            path,
            guard,
            budget,
        )?;
        Ok(ValidatedSerializedFieldGuard {
            handle: self.handle,
            lineage: Arc::clone(&self.lineage),
            schema_digest: self.schema_digest,
            previous_ordinal: self.previous_ordinal,
            operation_count: self.operation_count,
            next_operation_count,
            ordinal,
            path,
        })
    }

    /// Validates a replacement against a previously guarded candidate generation.
    pub fn prepare_validated_replace_field(
        &mut self,
        validated: ValidatedSerializedFieldGuard<'file>,
        replacement: UnityValue,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedSerializedFieldReplace<'_>, SerializedObjectEncodeError> {
        let same_object = Arc::ptr_eq(&validated.lineage, &self.lineage)
            && std::ptr::eq(validated.handle.file(), self.handle.file())
            && std::ptr::eq(validated.handle.info(), self.handle.info());
        if !same_object
            || validated.schema_digest != self.schema_digest
            || validated.previous_ordinal != self.previous_ordinal
            || validated.operation_count != self.operation_count
        {
            return Err(SerializedObjectEncodeError::StaleFieldGuard {
                path_id: self.path_id(),
                ordinal: validated.ordinal,
            });
        }
        let prepared = prepare_validated_field_replacement(
            FieldReplacementValidation {
                path_id: self.path_id(),
                schema: &self.schema,
                object_schema_digest: self.schema_digest,
                endian: self.endian,
                root: &mut self.root,
                previous_ordinal: self.previous_ordinal,
                validation_stats: self.validation_stats,
            },
            validated.ordinal,
            validated.path,
            replacement,
            budget,
        )?;
        Ok(PreparedSerializedFieldReplace {
            target: prepared.target,
            previous_ordinal: &mut self.previous_ordinal,
            validation_stats: &mut self.validation_stats,
            operation_count: &mut self.operation_count,
            replacement: prepared.replacement,
            ordinal: validated.ordinal,
            next_validation_stats: prepared.next_validation_stats,
            next_operation_count: validated.next_operation_count,
        })
    }

    /// Applies one operation immediately, preserving this candidate for later plan operations.
    pub fn apply(
        &mut self,
        operation: SerializedObjectMutation,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), SerializedObjectEncodeError> {
        apply_operation(
            operation,
            self.path_id(),
            &self.schema,
            self.schema_digest,
            self.endian,
            &mut self.root,
            &mut self.previous_ordinal,
            &mut self.validation_stats,
            budget,
        )?;
        self.operation_count = self
            .operation_count
            .checked_add(1)
            .ok_or(SerializedObjectEncodeError::OperationCountOverflow)?;
        Ok(())
    }

    /// Rewrites the fully validated candidate exactly once.
    pub fn finish(
        self,
        budget: &mut AssetLoadBudget,
    ) -> Result<EncodedSerializedObject, SerializedObjectEncodeError> {
        if self.operation_count == 0 {
            return Err(SerializedObjectEncodeError::NoOperations);
        }
        let Self {
            handle,
            schema,
            schema_digest,
            original,
            original_digest,
            root,
            endian,
            parse_stats,
            operation_count,
            validation_stats,
            ..
        } = self;
        let path_id = handle.path_id();
        let class_id = handle.class_id();
        let actual = root.kind();
        let UnityValue::Object(properties) = root else {
            return Err(SerializedObjectEncodeError::RootTypeInvariant { actual });
        };
        let (bytes, rewrite_stats) = rewrite_object(&schema, &properties, original, endian, budget)
            .map_err(|error| map_rewrite_error(path_id, error))?;
        let output_digest = DigestV1::hash_bytes(&bytes);
        let semantic_value = UnityValue::Object(properties);

        Ok(EncodedSerializedObject {
            path_id,
            class_id,
            mode: SerializedObjectEncodingMode::Semantic,
            schema_digest: Some(schema_digest),
            original_digest,
            output_digest,
            bytes,
            semantic_value: Some(semantic_value),
            stats: semantic_stats(
                parse_stats,
                validation_stats,
                rewrite_stats,
                operation_count,
            ),
        })
    }
}

struct FieldReplacementValidation<'schema, 'candidate> {
    path_id: i64,
    schema: &'schema TypeTreeSchema,
    object_schema_digest: DigestV1,
    endian: Endian,
    root: &'candidate mut UnityValue,
    previous_ordinal: Option<u32>,
    validation_stats: OperationValidationStats,
}

struct ValidatedFieldReplacement<'candidate> {
    target: &'candidate mut UnityValue,
    replacement: UnityValue,
    next_validation_stats: OperationValidationStats,
}

impl ValidatedFieldReplacement<'_> {
    fn commit(self) -> OperationValidationStats {
        *self.target = self.replacement;
        self.next_validation_stats
    }
}

fn prepare_field_replacement<'candidate>(
    state: FieldReplacementValidation<'_, 'candidate>,
    ordinal: u32,
    path: FieldPath,
    guard: SerializedFieldGuard,
    replacement: UnityValue,
    budget: &mut AssetLoadBudget,
) -> Result<ValidatedFieldReplacement<'candidate>, SerializedObjectEncodeError> {
    let path = validate_field_replacement_guard(
        state.schema,
        state.object_schema_digest,
        state.root,
        state.previous_ordinal,
        ordinal,
        path,
        guard,
        budget,
    )?;
    prepare_validated_field_replacement(state, ordinal, path, replacement, budget)
}

fn validate_field_replacement_guard(
    schema: &TypeTreeSchema,
    object_schema_digest: DigestV1,
    root: &UnityValue,
    previous_ordinal: Option<u32>,
    ordinal: u32,
    path: FieldPath,
    guard: SerializedFieldGuard,
    budget: &mut AssetLoadBudget,
) -> Result<FieldPath, SerializedObjectEncodeError> {
    if let Some(previous) = previous_ordinal
        && ordinal <= previous
    {
        return Err(SerializedObjectEncodeError::OperationOrder {
            previous,
            current: ordinal,
        });
    }
    if path.segments().is_empty() {
        return Err(SerializedObjectEncodeError::RootFieldReplacement { ordinal });
    }
    budget.consume_entries(1)?;
    charge_path(&path, budget)?;
    match schema_location_at_path(schema, root, &path) {
        Ok(_) => {}
        Err(failure) => return Err(map_path_failure(ordinal, path, failure)),
    }
    let current = match root.value_at_path(&path) {
        Ok(current) => current,
        Err(failure) => return Err(map_value_path_failure(ordinal, path, failure)),
    };
    if let Err(failure) = verify_field_guard(&path, guard, object_schema_digest, current, budget) {
        return Err(failure.into_error(ordinal, path));
    }

    Ok(path)
}

fn prepare_validated_field_replacement<'candidate>(
    state: FieldReplacementValidation<'_, 'candidate>,
    ordinal: u32,
    path: FieldPath,
    replacement: UnityValue,
    budget: &mut AssetLoadBudget,
) -> Result<ValidatedFieldReplacement<'candidate>, SerializedObjectEncodeError> {
    let resolution = match schema_location_at_path(state.schema, state.root, &path) {
        Ok(resolution) => resolution,
        Err(failure) => return Err(map_path_failure(ordinal, path, failure)),
    };
    let current = match state.root.value_at_path_mut(&path) {
        Ok(current) => current,
        Err(failure) => return Err(map_value_path_failure(ordinal, path, failure)),
    };

    let original = replace(current, replacement);
    let validation = resolution.semantic_owner.unwrap_or(SemanticOwnerLocation {
        schema: resolution.target,
        path_len: path.segments().len(),
    });
    let validation_result = match state
        .root
        .value_at_segments(&path.segments()[..validation.path_len])
    {
        Ok(candidate) => Ok(validate_value(
            state.schema,
            validation.schema.node,
            candidate,
            state.endian,
            budget,
            validation.schema.context,
            validation.schema.depth,
        )),
        Err(failure) => Err(failure),
    };

    let current = state
        .root
        .value_at_path_mut(&path)
        .expect("a terminal field replacement cannot invalidate its own path");
    let pending = replace(current, original);
    let traversal = match validation_result {
        Ok(Ok(stats)) => stats,
        Ok(Err(error)) => {
            return Err(map_operation_value_error(
                state.path_id,
                ordinal,
                path,
                error,
            ));
        }
        Err(failure) => return Err(map_value_path_failure(ordinal, path, failure)),
    };
    let mut next_validation_stats = state.validation_stats;
    next_validation_stats.record(traversal)?;
    let target = state
        .root
        .value_at_path_mut(&path)
        .expect("the validated field path remains stable until token commit");

    Ok(ValidatedFieldReplacement {
        target,
        replacement: pending,
        next_validation_stats,
    })
}

fn apply_operation(
    operation: SerializedObjectMutation,
    path_id: i64,
    schema: &TypeTreeSchema,
    object_schema_digest: DigestV1,
    endian: Endian,
    root: &mut UnityValue,
    previous_ordinal: &mut Option<u32>,
    validation_stats: &mut OperationValidationStats,
    budget: &mut AssetLoadBudget,
) -> Result<(), SerializedObjectEncodeError> {
    let ordinal = operation.ordinal;
    let field_replace = matches!(
        &operation.kind,
        SerializedObjectMutationKind::ReplaceField { .. }
    );
    if !field_replace {
        if let Some(previous) = *previous_ordinal
            && ordinal <= previous
        {
            return Err(SerializedObjectEncodeError::OperationOrder {
                previous,
                current: ordinal,
            });
        }
        budget.consume_entries(1)?;
    }

    match operation.kind {
        SerializedObjectMutationKind::ReplaceField {
            path,
            guard,
            replacement,
        } => {
            let validated = prepare_field_replacement(
                FieldReplacementValidation {
                    path_id,
                    schema,
                    object_schema_digest,
                    endian,
                    root,
                    previous_ordinal: *previous_ordinal,
                    validation_stats: *validation_stats,
                },
                ordinal,
                path,
                guard,
                replacement,
                budget,
            )?;
            *validation_stats = validated.commit();
        }
        SerializedObjectMutationKind::ReplaceObject { guard, replacement } => {
            verify_object_guard(ordinal, guard, object_schema_digest, root, budget)?;
            let replacement = UnityValue::Object(replacement);
            match validate_value(
                schema,
                schema.root(),
                &replacement,
                endian,
                budget,
                TypeTreeTraversalContext::root(),
                0,
            ) {
                Ok(stats) => validation_stats.record(stats)?,
                Err(error) => {
                    return Err(map_operation_value_error(
                        path_id,
                        ordinal,
                        FieldPath::root(),
                        error,
                    ));
                }
            }
            *root = replacement;
        }
        SerializedObjectMutationKind::EditSequence { path, guard, edit } => {
            charge_path(&path, budget)?;
            let location = match schema_location_at_path(schema, root, &path) {
                Ok(resolution) => resolution.target,
                Err(failure) => return Err(map_path_failure(ordinal, path, failure)),
            };
            let current = match root.value_at_path_mut(&path) {
                Ok(current) => current,
                Err(failure) => return Err(map_value_path_failure(ordinal, path, failure)),
            };
            if let Err(failure) =
                verify_field_guard(&path, guard, object_schema_digest, current, budget)
            {
                return Err(failure.into_error(ordinal, path));
            }
            let element = match location.node.semantic_layout() {
                SemanticLayout::Sequence(layout) | SemanticLayout::Map(layout) => layout.element(),
                layout => {
                    let error = TypeTreeWriteError::invalid_value(format!(
                        "TypeTree node '{}' has {:?} semantics, not an editable sequence",
                        location.node.name(),
                        layout.kind()
                    ));
                    return Err(map_operation_value_error(path_id, ordinal, path, error));
                }
            };
            if let SerializedSequenceEdit::Insert { value, .. }
            | SerializedSequenceEdit::Replace { value, .. } = &edit
            {
                let element_depth = location.depth.checked_add(1).ok_or(
                    SerializedObjectEncodeError::ArithmeticOverflow {
                        resource: "sequence element depth",
                    },
                )?;
                match validate_value(
                    schema,
                    element,
                    value,
                    endian,
                    budget,
                    location.context,
                    element_depth,
                ) {
                    Ok(stats) => validation_stats.record(stats)?,
                    Err(error) => {
                        return Err(map_operation_value_error(path_id, ordinal, path, error));
                    }
                }
            }
            apply_sequence_edit(ordinal, path, current, edit, budget)?;
        }
    }
    *previous_ordinal = Some(ordinal);
    Ok(())
}

fn verify_field_guard(
    path: &FieldPath,
    guard: SerializedFieldGuard,
    object_schema_digest: DigestV1,
    value: &UnityValue,
    budget: &mut AssetLoadBudget,
) -> Result<(), FieldGuardFailure> {
    let actual_schema = field_schema_digest(object_schema_digest, path)
        .map_err(map_guard_digest_error)
        .map_err(FieldGuardFailure::encoding)?;
    if guard.schema_digest != actual_schema {
        return Err(FieldGuardFailure::Schema {
            expected: guard.schema_digest,
            actual: actual_schema,
        });
    }
    let actual_value = semantic_value_digest(value, budget)
        .map_err(map_guard_digest_error)
        .map_err(FieldGuardFailure::encoding)?;
    if guard.value_digest != actual_value {
        return Err(FieldGuardFailure::Value {
            expected: guard.value_digest,
            actual: actual_value,
        });
    }
    Ok(())
}

enum FieldGuardFailure {
    Schema {
        expected: DigestV1,
        actual: DigestV1,
    },
    Value {
        expected: DigestV1,
        actual: DigestV1,
    },
    Encoding(Box<SerializedObjectEncodeError>),
}

impl FieldGuardFailure {
    fn encoding(error: SerializedObjectEncodeError) -> Self {
        Self::Encoding(Box::new(error))
    }

    fn into_error(self, ordinal: u32, path: FieldPath) -> SerializedObjectEncodeError {
        match self {
            Self::Schema { expected, actual } => {
                SerializedObjectEncodeError::FieldSchemaGuardMismatch {
                    ordinal,
                    path,
                    expected,
                    actual,
                }
            }
            Self::Value { expected, actual } => {
                SerializedObjectEncodeError::FieldValueGuardMismatch {
                    ordinal,
                    path,
                    expected,
                    actual,
                }
            }
            Self::Encoding(error) => *error,
        }
    }
}

fn verify_object_guard(
    ordinal: u32,
    guard: SerializedObjectGuard,
    actual_schema: DigestV1,
    value: &UnityValue,
    budget: &mut AssetLoadBudget,
) -> Result<(), SerializedObjectEncodeError> {
    if guard.schema_digest != actual_schema {
        return Err(SerializedObjectEncodeError::ObjectSchemaGuardMismatch {
            ordinal,
            expected: guard.schema_digest,
            actual: actual_schema,
        });
    }
    let actual_value = semantic_value_digest(value, budget).map_err(map_guard_digest_error)?;
    if guard.value_digest != actual_value {
        return Err(SerializedObjectEncodeError::ObjectValueGuardMismatch {
            ordinal,
            expected: guard.value_digest,
            actual: actual_value,
        });
    }
    Ok(())
}

fn apply_sequence_edit(
    ordinal: u32,
    path: FieldPath,
    value: &mut UnityValue,
    edit: SerializedSequenceEdit,
    budget: &mut AssetLoadBudget,
) -> Result<(), SerializedObjectEncodeError> {
    let actual = value.kind();
    let UnityValue::Array(values) = value else {
        return Err(SerializedObjectEncodeError::SequenceTypeMismatch {
            ordinal,
            path,
            actual,
        });
    };
    match edit {
        SerializedSequenceEdit::Insert { index, value } => {
            let index = match checked_sequence_index(index, values.len(), true) {
                Ok(index) => index,
                Err(failure) => return Err(failure.into_error(ordinal, path)),
            };
            let new_length = values.len().checked_add(1).ok_or(
                SerializedObjectEncodeError::ArithmeticOverflow {
                    resource: "sequence length",
                },
            )?;
            if new_length > MAX_TYPETREE_SEQUENCE_LENGTH {
                return Err(SerializedObjectEncodeError::SequenceLengthOverflow {
                    ordinal,
                    path,
                    length: new_length,
                    maximum: MAX_TYPETREE_SEQUENCE_LENGTH,
                });
            }
            reserve_sequence_slot(values, budget)?;
            values.insert(index, value);
        }
        SerializedSequenceEdit::Replace { index, value } => {
            let index = match checked_sequence_index(index, values.len(), false) {
                Ok(index) => index,
                Err(failure) => return Err(failure.into_error(ordinal, path)),
            };
            values[index] = value;
        }
        SerializedSequenceEdit::Remove { index } => {
            let index = match checked_sequence_index(index, values.len(), false) {
                Ok(index) => index,
                Err(failure) => return Err(failure.into_error(ordinal, path)),
            };
            values.remove(index);
        }
        SerializedSequenceEdit::Move { from, to } => {
            if from == to {
                return Err(SerializedObjectEncodeError::NoopSequenceMove {
                    ordinal,
                    path,
                    index: from,
                });
            }
            let from = match checked_sequence_index(from, values.len(), false) {
                Ok(index) => index,
                Err(failure) => return Err(failure.into_error(ordinal, path)),
            };
            let to = match checked_sequence_index(to, values.len(), false) {
                Ok(index) => index,
                Err(failure) => return Err(failure.into_error(ordinal, path)),
            };
            let value = values.remove(from);
            values.insert(to, value);
        }
        SerializedSequenceEdit::Clear => values.clear(),
    }
    Ok(())
}

fn checked_sequence_index(
    index: u32,
    length: usize,
    allow_end: bool,
) -> Result<usize, SequenceIndexFailure> {
    let converted = usize::try_from(index).map_err(|_| SequenceIndexFailure { index, length })?;
    let valid = if allow_end {
        converted <= length
    } else {
        converted < length
    };
    if !valid {
        return Err(SequenceIndexFailure { index, length });
    }
    Ok(converted)
}

struct SequenceIndexFailure {
    index: u32,
    length: usize,
}

impl SequenceIndexFailure {
    fn into_error(self, ordinal: u32, path: FieldPath) -> SerializedObjectEncodeError {
        SerializedObjectEncodeError::SequenceIndexOutOfBounds {
            ordinal,
            path,
            index: self.index,
            length: self.length,
        }
    }
}

fn reserve_sequence_slot(
    values: &mut Vec<UnityValue>,
    budget: &mut AssetLoadBudget,
) -> Result<(), SerializedObjectEncodeError> {
    if values.len() < values.capacity() {
        return Ok(());
    }
    let target_capacity =
        values
            .len()
            .checked_add(1)
            .ok_or(SerializedObjectEncodeError::ArithmeticOverflow {
                resource: "sequence capacity",
            })?;
    let allocation = target_capacity.checked_mul(size_of::<UnityValue>()).ok_or(
        SerializedObjectEncodeError::ArithmeticOverflow {
            resource: "sequence allocation",
        },
    )?;
    let allocation = usize_to_u64(allocation, "sequence allocation")?;
    budget.check_bytes(allocation)?;
    values
        .try_reserve_exact(1)
        .map_err(|source| SerializedObjectEncodeError::Allocation {
            resource: "sequence values",
            requested: target_capacity,
            source,
        })?;
    budget.consume_bytes(allocation)?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum PathFailure {
    SegmentOverflow,
    Missing {
        segment: u32,
    },
    SchemaMismatch {
        segment: u32,
    },
    TypeMismatch {
        segment: u32,
        expected: SerializedValueKind,
        actual: SerializedValueKind,
    },
    IndexOutOfBounds {
        segment: u32,
        index: u32,
        length: usize,
    },
}

impl From<PathFailure> for SerializedValueSchemaError {
    fn from(failure: PathFailure) -> Self {
        match failure {
            PathFailure::SegmentOverflow => Self::SegmentOverflow,
            PathFailure::Missing { segment } => Self::Missing { segment },
            PathFailure::SchemaMismatch { segment } => Self::SchemaMismatch { segment },
            PathFailure::TypeMismatch {
                segment,
                expected,
                actual,
            } => Self::TypeMismatch {
                segment,
                expected,
                actual,
            },
            PathFailure::IndexOutOfBounds {
                segment,
                index,
                length,
            } => Self::IndexOutOfBounds {
                segment,
                index,
                length,
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SchemaLocation<'schema> {
    node: SchemaNode<'schema>,
    context: TypeTreeTraversalContext,
    depth: u32,
}

#[derive(Debug, Clone, Copy)]
struct SemanticOwnerLocation<'schema> {
    schema: SchemaLocation<'schema>,
    path_len: usize,
}

#[derive(Debug, Clone, Copy)]
struct SchemaPathResolution<'schema> {
    target: SchemaLocation<'schema>,
    semantic_owner: Option<SemanticOwnerLocation<'schema>>,
}

fn schema_location_at_path<'schema>(
    schema: &'schema TypeTreeSchema,
    root: &UnityValue,
    path: &FieldPath,
) -> Result<SchemaPathResolution<'schema>, PathFailure> {
    let mut node = schema.root();
    let mut context = TypeTreeTraversalContext::root();
    let mut depth = 0_u32;
    let mut current = root;
    let mut nearest_owner = semantic_owner(node, context, depth, 0);

    for (segment_index, component) in path.segments().iter().enumerate() {
        let path_len = segment_index
            .checked_add(1)
            .ok_or(PathFailure::SegmentOverflow)?;
        let segment = u32::try_from(segment_index).map_err(|_| PathFailure::SegmentOverflow)?;
        let next_depth = depth.checked_add(1).ok_or(PathFailure::SegmentOverflow)?;
        let actual = current.kind();
        match component {
            FieldPathSegment::Field(name) => {
                let UnityValue::Object(fields) = current else {
                    return Err(PathFailure::TypeMismatch {
                        segment,
                        expected: SerializedValueKind::Object,
                        actual,
                    });
                };
                let next_value = fields.get(name).ok_or(PathFailure::Missing { segment })?;
                let (next_node, next_context) =
                    resolve_named_schema_child(schema, node, context, fields, name)
                        .ok_or(PathFailure::SchemaMismatch { segment })?;
                node = next_node;
                context = next_context;
                current = next_value;
            }
            FieldPathSegment::Index(index) => {
                let UnityValue::Array(values) = current else {
                    return Err(PathFailure::TypeMismatch {
                        segment,
                        expected: SerializedValueKind::Array,
                        actual,
                    });
                };
                let converted =
                    usize::try_from(*index).map_err(|_| PathFailure::IndexOutOfBounds {
                        segment,
                        index: *index,
                        length: values.len(),
                    })?;
                let next_value = values.get(converted).ok_or(PathFailure::IndexOutOfBounds {
                    segment,
                    index: *index,
                    length: values.len(),
                })?;
                node = match node.semantic_layout() {
                    SemanticLayout::Sequence(layout) | SemanticLayout::Map(layout) => {
                        layout.element()
                    }
                    SemanticLayout::Pair(layout) => match converted {
                        0 => layout.first(),
                        1 => layout.second(),
                        _ => return Err(PathFailure::SchemaMismatch { segment }),
                    },
                    _ => return Err(PathFailure::SchemaMismatch { segment }),
                };
                current = next_value;
            }
        }
        depth = next_depth;
        nearest_owner = semantic_owner(node, context, depth, path_len).or(nearest_owner);
    }

    Ok(SchemaPathResolution {
        target: SchemaLocation {
            node,
            context,
            depth,
        },
        semantic_owner: nearest_owner,
    })
}

fn semantic_owner<'schema>(
    node: SchemaNode<'schema>,
    context: TypeTreeTraversalContext,
    depth: u32,
    path_len: usize,
) -> Option<SemanticOwnerLocation<'schema>> {
    matches!(
        node.semantic_layout(),
        SemanticLayout::PPtr(_) | SemanticLayout::ReferencedObject(_)
    )
    .then_some(SemanticOwnerLocation {
        schema: SchemaLocation {
            node,
            context,
            depth,
        },
        path_len,
    })
}

fn resolve_named_schema_child<'schema>(
    schema: &'schema TypeTreeSchema,
    node: SchemaNode<'schema>,
    mut context: TypeTreeTraversalContext,
    object: &IndexMap<String, UnityValue>,
    name: &str,
) -> Option<(SchemaNode<'schema>, TypeTreeTraversalContext)> {
    let runtime_type = match node.semantic_layout() {
        SemanticLayout::ReferencedObject(layout) => {
            managed_reference_type_from_value(layout, object)
        }
        _ => None,
    };
    for child in node.children() {
        let Some(child_context) = context.descend(node, child) else {
            continue;
        };
        if child.name() != name {
            continue;
        }
        let resolved = match node.semantic_layout() {
            SemanticLayout::ReferencedObject(layout) if layout.is_payload(child) => {
                resolve_managed_payload_for_type(schema, layout, runtime_type?)?
            }
            _ => child,
        };
        return Some((resolved, child_context));
    }
    None
}

fn resolve_named_schema_child_for_managed_type<'schema>(
    schema: &'schema TypeTreeSchema,
    node: SchemaNode<'schema>,
    mut context: TypeTreeTraversalContext,
    name: &str,
    runtime_type: SerializedManagedReferenceType<'_>,
) -> Option<(SchemaNode<'schema>, TypeTreeTraversalContext)> {
    for child in node.children() {
        let Some(child_context) = context.descend(node, child) else {
            continue;
        };
        if child.name() != name {
            continue;
        }
        let resolved = match node.semantic_layout() {
            SemanticLayout::ReferencedObject(layout) if layout.is_payload(child) => {
                resolve_managed_payload_for_type(schema, layout, runtime_type)?
            }
            _ => child,
        };
        return Some((resolved, child_context));
    }
    None
}

fn managed_reference_type_from_value<'value>(
    layout: unity_asset_binary::typetree::ReferencedObjectLayout<'_>,
    object: &'value IndexMap<String, UnityValue>,
) -> Option<SerializedManagedReferenceType<'value>> {
    let UnityValue::Object(type_fields) = object.get(layout.type_node().name())? else {
        return None;
    };
    let UnityValue::String(class_name) = type_fields.get(layout.class_field().name())? else {
        return None;
    };
    let UnityValue::String(namespace) = type_fields.get(layout.namespace_field().name())? else {
        return None;
    };
    let UnityValue::String(assembly_name) = type_fields.get(layout.assembly_field().name())? else {
        return None;
    };
    Some(SerializedManagedReferenceType::new(
        class_name,
        namespace,
        assembly_name,
    ))
}

fn resolve_managed_payload_for_type<'schema>(
    schema: &'schema TypeTreeSchema,
    layout: unity_asset_binary::typetree::ReferencedObjectLayout<'schema>,
    runtime_type: SerializedManagedReferenceType<'_>,
) -> Option<SchemaNode<'schema>> {
    schema
        .resolve_managed_root(
            runtime_type.class_name(),
            runtime_type.namespace(),
            runtime_type.assembly_name(),
        )
        .or_else(|| layout.payload().fallback())
}

fn map_path_failure(
    ordinal: u32,
    path: FieldPath,
    failure: PathFailure,
) -> SerializedObjectEncodeError {
    match failure {
        PathFailure::SegmentOverflow => SerializedObjectEncodeError::ArithmeticOverflow {
            resource: "field path segment",
        },
        PathFailure::Missing { segment } => SerializedObjectEncodeError::PathMissing {
            ordinal,
            path,
            segment,
        },
        PathFailure::SchemaMismatch { segment } => {
            SerializedObjectEncodeError::PathSchemaMismatch {
                ordinal,
                path,
                segment,
            }
        }
        PathFailure::TypeMismatch {
            segment,
            expected,
            actual,
        } => SerializedObjectEncodeError::PathTypeMismatch {
            ordinal,
            path,
            segment,
            expected,
            actual,
        },
        PathFailure::IndexOutOfBounds {
            segment,
            index,
            length,
        } => SerializedObjectEncodeError::PathIndexOutOfBounds {
            ordinal,
            path,
            segment,
            index,
            length,
        },
    }
}

fn map_value_path_failure(
    ordinal: u32,
    path: FieldPath,
    failure: ValuePathError,
) -> SerializedObjectEncodeError {
    let Some(segment) = failure.segment() else {
        return SerializedObjectEncodeError::RootFieldReplacement { ordinal };
    };
    let segment = match u32::try_from(segment) {
        Ok(segment) => segment,
        Err(_) => {
            return SerializedObjectEncodeError::ArithmeticOverflow {
                resource: "field path segment",
            };
        }
    };
    match failure {
        ValuePathError::ClassRoot => SerializedObjectEncodeError::RootFieldReplacement { ordinal },
        ValuePathError::MissingField { .. } => SerializedObjectEncodeError::PathMissing {
            ordinal,
            path,
            segment,
        },
        ValuePathError::ExpectedObject { actual, .. } => {
            SerializedObjectEncodeError::PathTypeMismatch {
                ordinal,
                path,
                segment,
                expected: SerializedValueKind::Object,
                actual,
            }
        }
        ValuePathError::ExpectedArray { actual, .. } => {
            SerializedObjectEncodeError::PathTypeMismatch {
                ordinal,
                path,
                segment,
                expected: SerializedValueKind::Array,
                actual,
            }
        }
        ValuePathError::IndexOutOfBounds { index, length, .. } => {
            SerializedObjectEncodeError::PathIndexOutOfBounds {
                ordinal,
                path,
                segment,
                index,
                length,
            }
        }
    }
}

fn charge_path(
    path: &FieldPath,
    budget: &mut AssetLoadBudget,
) -> Result<(), SerializedObjectEncodeError> {
    let members = usize_to_u64(path.segments().len(), "field path members")?;
    budget.consume_members(members)?;
    budget.observe_depth(u32::try_from(path.segments().len()).map_err(|_| {
        SerializedObjectEncodeError::ArithmeticOverflow {
            resource: "field path depth",
        }
    })?)?;
    Ok(())
}

fn semantic_stats(
    parse: TypeTreeTraversalStats,
    validation: OperationValidationStats,
    rewrite: TemplateRewriteStats,
    operations_applied: u64,
) -> SerializedObjectEncodingStats {
    SerializedObjectEncodingStats {
        parse_passes: 1,
        validation_passes: validation.passes,
        rewrite_passes: 1,
        operations_applied,
        parse,
        validation: validation.traversal,
        rewrite_input: rewrite.input,
        rewrite_output: rewrite.output,
        preserved_bytes: rewrite.preserved_bytes,
    }
}

fn endian_for(byte_order: ByteOrder) -> Endian {
    match byte_order {
        ByteOrder::Big => Endian::Big,
        ByteOrder::Little => Endian::Little,
    }
}

enum BinaryStage {
    Schema,
    Parse,
}

fn map_binary_error(
    path_id: i64,
    stage: BinaryStage,
    error: BinaryError,
) -> SerializedObjectEncodeError {
    match error {
        BinaryError::Budget(error) => SerializedObjectEncodeError::Budget(error),
        source => match stage {
            BinaryStage::Schema => SerializedObjectEncodeError::Schema { path_id, source },
            BinaryStage::Parse => SerializedObjectEncodeError::Parse { path_id, source },
        },
    }
}

fn map_schema_digest_error(
    path_id: i64,
    error: TypeTreeSemanticDigestError,
) -> SerializedObjectEncodeError {
    match error {
        TypeTreeSemanticDigestError::Budget(error) => SerializedObjectEncodeError::Budget(error),
        source => SerializedObjectEncodeError::SchemaDigest { path_id, source },
    }
}

fn map_operation_value_error(
    path_id: i64,
    ordinal: u32,
    path: FieldPath,
    error: TypeTreeWriteError,
) -> SerializedObjectEncodeError {
    match error {
        TypeTreeWriteError::Shape {
            expected_fields,
            actual_fields,
        } => SerializedObjectEncodeError::ReplacementShape {
            path_id,
            ordinal,
            path,
            expected_fields,
            actual_fields,
        },
        TypeTreeWriteError::Budget { source, .. } => SerializedObjectEncodeError::Budget(source),
        source => SerializedObjectEncodeError::ReplacementValue {
            path_id,
            ordinal,
            path,
            source,
        },
    }
}

fn map_rewrite_error(path_id: i64, error: TypeTreeWriteError) -> SerializedObjectEncodeError {
    match error {
        TypeTreeWriteError::Budget { source, .. } => SerializedObjectEncodeError::Budget(source),
        source => SerializedObjectEncodeError::Rewrite { path_id, source },
    }
}

fn map_guard_digest_error(error: SemanticDigestError) -> SerializedObjectEncodeError {
    match error {
        SemanticDigestError::Budget(error) => SerializedObjectEncodeError::Budget(error),
        source => SerializedObjectEncodeError::GuardDigest { source },
    }
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, SerializedObjectEncodeError> {
    u64::try_from(value).map_err(|_| SerializedObjectEncodeError::ArithmeticOverflow { resource })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typetree::test_support::{node, record};
    use unity_asset_binary::asset::SerializedType;
    use unity_asset_binary::typetree::{TypeTree, TypeTreeNode};

    fn compile(root: TypeTreeNode, referenced_types: &[SerializedType]) -> TypeTreeSchema {
        let mut tree = TypeTree::new();
        tree.add_node(root);
        TypeTreeSchema::compile(&tree, referenced_types, &mut AssetLoadBudget::default())
            .expect("compile test TypeTree")
    }

    fn schema_digest(schema: &TypeTreeSchema) -> DigestV1 {
        schema
            .semantic_digest_with_budget(&mut AssetLoadBudget::default())
            .expect("digest test TypeTree")
    }

    #[test]
    fn write_shape_error_maps_to_operation_context() {
        let path = FieldPath::root();
        let error = map_operation_value_error(41, 7, path.clone(), TypeTreeWriteError::shape(2, 3));

        assert!(matches!(
            error,
            SerializedObjectEncodeError::ReplacementShape {
                path_id: 41,
                ordinal: 7,
                path: actual_path,
                expected_fields: 2,
                actual_fields: 3,
            } if actual_path == path
        ));
    }

    #[test]
    fn write_budget_error_maps_to_public_budget() {
        let error = map_operation_value_error(
            41,
            7,
            FieldPath::root(),
            TypeTreeWriteError::budget(
                "validate replacement",
                BudgetError::Exceeded {
                    resource: "entries",
                    limit: 1,
                    requested: 2,
                },
            ),
        );

        assert!(matches!(
            error,
            SerializedObjectEncodeError::Budget(BudgetError::Exceeded {
                resource: "entries",
                limit: 1,
                requested: 2,
            })
        ));
    }

    #[test]
    fn rewrite_budget_error_maps_to_public_budget() {
        let error = map_rewrite_error(
            41,
            TypeTreeWriteError::budget(
                "rewrite object",
                BudgetError::Exceeded {
                    resource: "bytes",
                    limit: 3,
                    requested: 4,
                },
            ),
        );

        assert!(matches!(
            error,
            SerializedObjectEncodeError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 3,
                requested: 4,
            })
        ));
    }

    #[test]
    fn opaque_schema_projection_exposes_stable_pptr_field_locations() {
        let mut pointer = node("PPtr<Texture2D>", "m_Texture");
        pointer.children = vec![
            node("UInt16", "m_Tag"),
            node("UInt32", "m_FileID"),
            node("UInt64", "m_PathID"),
        ];
        let schema = compile(record(vec![pointer]), &[]);
        let root = SerializedValueSchema {
            schema: &schema,
            node: schema.root(),
            context: TypeTreeTraversalContext::root(),
        };

        let layout = root
            .field("m_Texture")
            .and_then(SerializedValueSchema::pptr_layout)
            .expect("compiled PPtr layout");

        assert_eq!(layout.file_field(), "m_FileID");
        assert_eq!(layout.file_index(), 1);
        assert_eq!(layout.file_primitive(), PrimitiveKind::U32);
        assert_eq!(layout.path_field(), "m_PathID");
        assert_eq!(layout.path_index(), 2);
        assert_eq!(layout.path_primitive(), PrimitiveKind::U64);
        assert_eq!(layout.field_count(), 3);
    }

    #[test]
    fn field_validation_includes_nearest_pptr_owner() {
        let mut pointer = node("PPtr<Texture2D>", "m_Texture");
        pointer.children = vec![node("UInt32", "m_FileID"), node("UInt64", "m_PathID")];
        let schema = compile(record(vec![pointer]), &[]);
        let digest = schema_digest(&schema);
        let path = FieldPath::root()
            .push_field("m_Texture")
            .and_then(|path| path.push_field("m_FileID"))
            .expect("valid field path");
        let original_file_id = UnityValue::Unsigned(0);
        let mut guard_budget = AssetLoadBudget::default();
        let guard = SerializedFieldGuard::from_observed(
            digest,
            &path,
            &original_file_id,
            &mut guard_budget,
        )
        .expect("build field guard");
        let mut root = UnityValue::Object(IndexMap::from([(
            "m_Texture".to_owned(),
            UnityValue::Object(IndexMap::from([
                ("m_FileID".to_owned(), original_file_id),
                ("m_PathID".to_owned(), UnityValue::Unsigned(0)),
            ])),
        )]));
        let operation = SerializedObjectMutation::replace_field(
            7,
            path.clone(),
            guard,
            UnityValue::Unsigned(i32::MAX as u64 + 1),
        );
        let mut previous = None;
        let mut validation = OperationValidationStats::default();
        let mut budget = AssetLoadBudget::default();
        let original_root = root.clone();

        let error = apply_operation(
            operation,
            91,
            &schema,
            digest,
            Endian::Little,
            &mut root,
            &mut previous,
            &mut validation,
            &mut budget,
        )
        .expect_err("PPtr role must reject a leaf-valid UInt32");

        match error {
            SerializedObjectEncodeError::ReplacementValue {
                path_id,
                ordinal,
                path: actual_path,
                source,
            } => {
                assert_eq!(path_id, 91);
                assert_eq!(ordinal, 7);
                assert_eq!(actual_path, path);
                assert!(source.to_string().contains("file ID must fit in i32"));
            }
            other => panic!("unexpected encoder error: {other}"),
        }
        assert_eq!(previous, None);
        assert_eq!(validation.passes, 0);
        assert_eq!(root, original_root);
    }

    #[test]
    fn prepared_field_replacement_is_inert_until_commit() {
        let schema = compile(record(vec![node("UInt32", "m_Value")]), &[]);
        let digest = schema_digest(&schema);
        let path = FieldPath::root().push_field("m_Value").unwrap();
        let original = UnityValue::Unsigned(7);
        let guard = SerializedFieldGuard::from_observed(
            digest,
            &path,
            &original,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let mut root = UnityValue::Object(IndexMap::from([("m_Value".to_owned(), original)]));
        let mut previous = None;
        let mut validation = OperationValidationStats::default();
        let mut operation_count = 0_u64;

        let validated = prepare_field_replacement(
            FieldReplacementValidation {
                path_id: 93,
                schema: &schema,
                object_schema_digest: digest,
                endian: Endian::Little,
                root: &mut root,
                previous_ordinal: previous,
                validation_stats: validation,
            },
            7,
            path.clone(),
            guard,
            UnityValue::Unsigned(9),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let pending = PreparedSerializedFieldReplace {
            target: validated.target,
            previous_ordinal: &mut previous,
            validation_stats: &mut validation,
            operation_count: &mut operation_count,
            replacement: validated.replacement,
            ordinal: 7,
            next_validation_stats: validated.next_validation_stats,
            next_operation_count: 1,
        };
        drop(pending);
        assert_eq!(root.value_at_path(&path).unwrap(), &UnityValue::Unsigned(7));
        assert_eq!(previous, None);
        assert_eq!(validation.passes, 0);
        assert_eq!(operation_count, 0);

        let validated = prepare_field_replacement(
            FieldReplacementValidation {
                path_id: 93,
                schema: &schema,
                object_schema_digest: digest,
                endian: Endian::Little,
                root: &mut root,
                previous_ordinal: previous,
                validation_stats: validation,
            },
            7,
            path.clone(),
            guard,
            UnityValue::Unsigned(9),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        PreparedSerializedFieldReplace {
            target: validated.target,
            previous_ordinal: &mut previous,
            validation_stats: &mut validation,
            operation_count: &mut operation_count,
            replacement: validated.replacement,
            ordinal: 7,
            next_validation_stats: validated.next_validation_stats,
            next_operation_count: 1,
        }
        .commit();

        assert_eq!(root.value_at_path(&path).unwrap(), &UnityValue::Unsigned(9));
        assert_eq!(previous, Some(7));
        assert_eq!(validation.passes, 1);
        assert_eq!(operation_count, 1);
    }

    #[test]
    fn discriminator_validation_includes_referenced_object_owner() {
        let mut managed_type = SerializedType::new(114);
        managed_type.class_name = "C".to_owned();
        managed_type.namespace = "N".to_owned();
        managed_type.assembly_name = "A".to_owned();
        managed_type
            .type_tree
            .add_node(record(vec![node("UInt8", "m_Value")]));

        let mut type_node = node("ReferencedObjectType", "type");
        type_node.children = vec![
            node("string", "class"),
            node("string", "ns"),
            node("string", "asm"),
        ];
        let mut referenced = node("ReferencedObject", "m_Ref");
        referenced.children = vec![type_node, node("ReferencedObjectData", "data")];
        let schema = compile(record(vec![referenced]), &[managed_type]);
        let digest = schema_digest(&schema);
        let path = FieldPath::root()
            .push_field("m_Ref")
            .and_then(|path| path.push_field("type"))
            .and_then(|path| path.push_field("class"))
            .expect("valid discriminator path");
        let original_class = UnityValue::String("C".to_owned());
        let mut guard_budget = AssetLoadBudget::default();
        let guard =
            SerializedFieldGuard::from_observed(digest, &path, &original_class, &mut guard_budget)
                .expect("build discriminator guard");
        let mut root = UnityValue::Object(IndexMap::from([(
            "m_Ref".to_owned(),
            UnityValue::Object(IndexMap::from([
                (
                    "type".to_owned(),
                    UnityValue::Object(IndexMap::from([
                        ("class".to_owned(), original_class),
                        ("ns".to_owned(), UnityValue::String("N".to_owned())),
                        ("asm".to_owned(), UnityValue::String("A".to_owned())),
                    ])),
                ),
                (
                    "data".to_owned(),
                    UnityValue::Object(IndexMap::from([(
                        "m_Value".to_owned(),
                        UnityValue::Integer(7),
                    )])),
                ),
            ])),
        )]));
        let operation = SerializedObjectMutation::replace_field(
            11,
            path.clone(),
            guard,
            UnityValue::String("Missing".to_owned()),
        );
        let mut previous = None;
        let mut validation = OperationValidationStats::default();
        let mut budget = AssetLoadBudget::default();
        let original_root = root.clone();

        let error = apply_operation(
            operation,
            92,
            &schema,
            digest,
            Endian::Little,
            &mut root,
            &mut previous,
            &mut validation,
            &mut budget,
        )
        .expect_err("unresolved managed discriminator must fail its operation");

        match error {
            SerializedObjectEncodeError::ReplacementValue {
                path_id,
                ordinal,
                path: actual_path,
                source,
            } => {
                assert_eq!(path_id, 92);
                assert_eq!(ordinal, 11);
                assert_eq!(actual_path, path);
                assert!(
                    source
                        .to_string()
                        .contains("has no schema or writable fallback")
                );
            }
            other => panic!("unexpected encoder error: {other}"),
        }
        assert_eq!(previous, None);
        assert_eq!(validation.passes, 0);
        assert_eq!(root, original_root);
    }
}
