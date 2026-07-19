//! Atomic, schema-aware encoding for one SerializedFile object.

use std::collections::TryReserveError;
use std::error::Error as StdError;
use std::mem::{replace, size_of};

use indexmap::IndexMap;
use thiserror::Error;
use unity_asset_binary::asset::SerializedFile;
use unity_asset_binary::error::BinaryError;
use unity_asset_binary::object::ObjectHandle;
use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{
    SchemaNode, SemanticLayout, TypeTreeParseMode, TypeTreeParseOptions, TypeTreeSchema,
    TypeTreeSemanticDigestError, TypeTreeTraversalContext, TypeTreeTraversalStats,
};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, DigestV1, FieldPath, FieldPathSegment, SemanticDigestError,
    UnityAssetError, UnityValue, ValuePathError, field_schema_digest, semantic_value_digest,
};

/// Writer-facing alias for the shared stable [`UnityValue`] shape discriminator.
pub use unity_asset_core::UnityValueKind as SerializedValueKind;

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

    #[must_use]
    pub const fn stats(&self) -> SerializedObjectEncodingStats {
        self.stats
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
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
        source: UnityAssetError,
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
        source: UnityAssetError,
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

        let parse_stats = parsed.stats;
        let mut root = UnityValue::Object(parsed.properties);
        let endian = endian_for(self.handle.file().header.byte_order());
        let mut previous_ordinal = None;
        let mut operation_count = 0_u64;
        let mut validation_stats = OperationValidationStats::default();
        for operation in std::iter::once(first).chain(operations) {
            apply_operation(
                operation,
                path_id,
                &schema,
                schema_digest,
                endian,
                &mut root,
                &mut previous_ordinal,
                &mut validation_stats,
                budget,
            )?;
            operation_count = operation_count
                .checked_add(1)
                .ok_or(SerializedObjectEncodeError::OperationCountOverflow)?;
        }
        let actual = root.kind();
        let UnityValue::Object(properties) = root else {
            return Err(SerializedObjectEncodeError::RootTypeInvariant { actual });
        };
        let (bytes, rewrite_stats) = rewrite_object(&schema, &properties, original, endian, budget)
            .map_err(|error| map_rewrite_error(path_id, error))?;
        let output_digest = DigestV1::hash_bytes(&bytes);

        Ok(EncodedSerializedObject {
            path_id,
            class_id,
            mode: SerializedObjectEncodingMode::Semantic,
            schema_digest: Some(schema_digest),
            original_digest,
            output_digest,
            bytes,
            stats: semantic_stats(
                parse_stats,
                validation_stats,
                rewrite_stats,
                operation_count,
            ),
        })
    }

    /// Replaces raw bytes only after verifying the original digest and explicit acknowledgement.
    pub fn encode_unsafe_raw(
        self,
        replacement: UnsafeRawObjectReplacement,
        budget: &mut AssetLoadBudget,
    ) -> Result<EncodedSerializedObject, SerializedObjectEncodeError> {
        let path_id = self.path_id();
        let class_id = self.class_id();
        let original = self
            .handle
            .raw_data()
            .map_err(|source| SerializedObjectEncodeError::ReadRaw { path_id, source })?;
        let actual = DigestV1::hash_bytes(original);
        if replacement.expected_raw_digest != actual {
            return Err(SerializedObjectEncodeError::RawDigestMismatch {
                path_id,
                expected: replacement.expected_raw_digest,
                actual,
            });
        }
        let UnsafeRawObjectReplacement {
            bytes,
            acknowledgement:
                UnsafeRawObjectAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
            ..
        } = replacement;
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
            original_digest: actual,
            output_digest,
            bytes,
            stats: SerializedObjectEncodingStats {
                operations_applied: 1,
                ..SerializedObjectEncodingStats::default()
            },
        })
    }
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
    if let Some(previous) = *previous_ordinal
        && ordinal <= previous
    {
        return Err(SerializedObjectEncodeError::OperationOrder {
            previous,
            current: ordinal,
        });
    }
    budget.consume_entries(1)?;

    match operation.kind {
        SerializedObjectMutationKind::ReplaceField {
            path,
            guard,
            replacement,
        } => {
            if path.segments().is_empty() {
                return Err(SerializedObjectEncodeError::RootFieldReplacement { ordinal });
            }
            charge_path(&path, budget)?;
            let resolution = match schema_location_at_path(schema, root, &path) {
                Ok(resolution) => resolution,
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
            let original = replace(current, replacement);
            let validation = resolution.semantic_owner.unwrap_or(SemanticOwnerLocation {
                schema: resolution.target,
                path_len: path.segments().len(),
            });
            let validation_result =
                match root.value_at_segments(&path.segments()[..validation.path_len]) {
                    Ok(candidate) => Ok(validate_value(
                        schema,
                        validation.schema.node,
                        candidate,
                        endian,
                        budget,
                        validation.schema.context,
                        validation.schema.depth,
                    )),
                    Err(failure) => Err(failure),
                };

            // Only the terminal value changed, so the resolved path remains valid for rollback.
            let current = root
                .value_at_path_mut(&path)
                .expect("a terminal field replacement cannot invalidate its own path");
            let pending = replace(current, original);
            match validation_result {
                Ok(Ok(stats)) => {
                    validation_stats.record(stats)?;
                    *current = pending;
                }
                Ok(Err(error)) => {
                    return Err(map_operation_value_error(path_id, ordinal, path, error));
                }
                Err(failure) => return Err(map_value_path_failure(ordinal, path, failure)),
            }
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
                    let error = UnityAssetError::format(format!(
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
        .map_err(FieldGuardFailure::Encoding)?;
    if guard.schema_digest != actual_schema {
        return Err(FieldGuardFailure::Schema {
            expected: guard.schema_digest,
            actual: actual_schema,
        });
    }
    let actual_value = semantic_value_digest(value, budget)
        .map_err(map_guard_digest_error)
        .map_err(FieldGuardFailure::Encoding)?;
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
    Encoding(SerializedObjectEncodeError),
}

impl FieldGuardFailure {
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
            Self::Encoding(error) => error,
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
    for child in node.children() {
        let Some(child_context) = context.descend(node, child) else {
            continue;
        };
        if child.name() != name {
            continue;
        }
        let resolved = match node.semantic_layout() {
            SemanticLayout::ReferencedObject(layout) if layout.is_payload(child) => {
                resolve_managed_payload(schema, layout, object)?
            }
            _ => child,
        };
        return Some((resolved, child_context));
    }
    None
}

fn resolve_managed_payload<'schema>(
    schema: &'schema TypeTreeSchema,
    layout: unity_asset_binary::typetree::ReferencedObjectLayout<'schema>,
    object: &IndexMap<String, UnityValue>,
) -> Option<SchemaNode<'schema>> {
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
    schema
        .resolve_managed_root(class_name, namespace, assembly_name)
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
    error: UnityAssetError,
) -> SerializedObjectEncodeError {
    match error {
        UnityAssetError::TypeTreeShape {
            expected_fields,
            actual_fields,
        } => SerializedObjectEncodeError::ReplacementShape {
            path_id,
            ordinal,
            path,
            expected_fields,
            actual_fields,
        },
        error => match budget_from_error(&error) {
            Some(error) => SerializedObjectEncodeError::Budget(error),
            None => SerializedObjectEncodeError::ReplacementValue {
                path_id,
                ordinal,
                path,
                source: error,
            },
        },
    }
}

fn map_rewrite_error(path_id: i64, error: UnityAssetError) -> SerializedObjectEncodeError {
    match budget_from_error(&error) {
        Some(error) => SerializedObjectEncodeError::Budget(error),
        None => SerializedObjectEncodeError::Rewrite {
            path_id,
            source: error,
        },
    }
}

fn map_guard_digest_error(error: SemanticDigestError) -> SerializedObjectEncodeError {
    match error {
        SemanticDigestError::Budget(error) => SerializedObjectEncodeError::Budget(error),
        source => SerializedObjectEncodeError::GuardDigest { source },
    }
}

fn budget_from_error(error: &(dyn StdError + 'static)) -> Option<BudgetError> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(budget) = error.downcast_ref::<BudgetError>() {
            return Some(budget.clone());
        }
        current = error.source();
    }
    None
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
