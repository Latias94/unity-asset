use std::mem::{size_of, take};
use std::sync::Arc;

use thiserror::Error;
use unity_asset_core::{
    AllocationSizeError, AssetLoadBudget, BudgetError, DigestBuildError, DigestV1, FieldPath,
    ObjectId, ObjectKind, SemanticDigestError, SourceId, UnityClass, UnityValue,
    UnityValueCloneError, UnityValueKind, ValuePathError, arc_value_allocation_bytes,
    semantic_value_digest, yaml_field_schema_digest, yaml_schema_digest,
};

use crate::schema::SchemaProvenance;

use super::super::{FieldGuard, ObjectGuard, WorkspaceObject, WorkspaceObjectValue};

/// Completed semantic YAML candidate awaiting encoding into an exact artifact.
#[derive(Debug)]
pub(super) struct FinishedYamlObject {
    document_index: usize,
    class: UnityClass,
}

impl FinishedYamlObject {
    pub(super) const fn document_index(&self) -> usize {
        self.document_index
    }

    pub(super) const fn class(&self) -> &UnityClass {
        &self.class
    }
}

/// One owned, copy-on-write YAML object receiving mutations in plan order.
#[derive(Debug)]
#[must_use = "finish the candidate to produce a prepared workspace object"]
pub(super) struct YamlObjectCandidate {
    object: ObjectId,
    lineage: Arc<CandidateLineage>,
    document_index: usize,
    class: UnityClass,
    previous_ordinal: Option<u32>,
}

#[derive(Debug)]
struct CandidateLineage;

/// A field guard bound to one unchanged YAML candidate generation.
#[derive(Debug)]
#[must_use = "prepare or discard the validated field guard"]
pub(super) struct ValidatedYamlFieldGuard {
    source: SourceId,
    lineage: Arc<CandidateLineage>,
    document_index: usize,
    previous_ordinal: Option<u32>,
    ordinal: u32,
    path: FieldPath,
}

impl ValidatedYamlFieldGuard {
    #[must_use]
    pub(super) const fn path(&self) -> &FieldPath {
        &self.path
    }
}

/// A fully validated YAML field replacement awaiting an infallible candidate commit.
#[derive(Debug)]
#[must_use = "commit the prepared field replacement after dependent allocations succeed"]
pub(super) struct PreparedYamlFieldReplace<'candidate> {
    target: &'candidate mut UnityValue,
    previous_ordinal: &'candidate mut Option<u32>,
    replacement: UnityValue,
    ordinal: u32,
}

impl PreparedYamlFieldReplace<'_> {
    /// Installs the already validated replacement without allocation or further path resolution.
    pub(super) fn commit(self) {
        *self.target = self.replacement;
        *self.previous_ordinal = Some(self.ordinal);
    }
}

impl YamlObjectCandidate {
    /// Consumes a workspace object and clones its YAML class exactly once into budgeted storage.
    pub(super) fn from_workspace_object(
        base: WorkspaceObject,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, YamlCandidateBuildError> {
        let (handle, value, schema) = base.into_shared_parts();
        let object = handle.into_object();
        let WorkspaceObjectValue::Yaml(yaml) = value else {
            return Err(YamlCandidateBuildError::WorkspaceValueKindMismatch);
        };
        Self::from_class(object, yaml.document_index(), schema, yaml.class(), budget)
    }

    /// Builds a candidate from already resolved YAML identity and schema state.
    pub(super) fn from_class(
        object: ObjectId,
        document_index: usize,
        schema: Arc<SchemaProvenance>,
        base: &UnityClass,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, YamlCandidateBuildError> {
        if object.kind() != ObjectKind::Yaml {
            return Err(YamlCandidateBuildError::ObjectKindMismatch {
                actual: object.kind(),
            });
        }
        if schema.object_kind() != ObjectKind::Yaml {
            return Err(YamlCandidateBuildError::SchemaKindMismatch {
                actual: schema.object_kind(),
            });
        }
        if schema.class_id() != base.class_id {
            return Err(YamlCandidateBuildError::SchemaClassMismatch {
                schema: schema.class_id(),
                class: base.class_id,
            });
        }
        if !yaml_identity_matches(&object, document_index, base) {
            return Err(YamlCandidateBuildError::IdentityMismatch { document_index });
        }
        let class = base
            .try_clone_with_budget(budget)
            .map_err(YamlCandidateBuildError::Clone)?;
        let lineage_bytes = arc_value_allocation_bytes::<CandidateLineage>()?;
        budget.consume_bytes(lineage_bytes)?;
        Ok(Self {
            object,
            lineage: Arc::new(CandidateLineage),
            document_index,
            class,
            previous_ordinal: None,
        })
    }

    pub(super) const fn object(&self) -> &ObjectId {
        &self.object
    }

    #[cfg(test)]
    pub(super) const fn document_index(&self) -> usize {
        self.document_index
    }

    pub(super) const fn class(&self) -> &UnityClass {
        &self.class
    }

    /// Verifies an ordered field guard without borrowing or changing the candidate.
    ///
    /// The returned token is bound to this candidate's source, document, and operation cursor.
    /// This lets callers perform dependent allocations after the guard succeeds while preventing
    /// a token from being replayed after another operation changes the staged object.
    pub(super) fn validate_replace_field_guard(
        &self,
        ordinal: u32,
        path: FieldPath,
        guard: FieldGuard,
        budget: &mut AssetLoadBudget,
    ) -> Result<ValidatedYamlFieldGuard, YamlCandidateError> {
        if let Some(previous) = self.previous_ordinal
            && ordinal <= previous
        {
            return Err(YamlCandidateError::OperationOrder {
                ordinal,
                document_index: self.document_index,
                previous,
            });
        }
        if path.segments().is_empty() {
            return Err(YamlCandidateError::RootFieldPath {
                ordinal,
                document_index: self.document_index,
            });
        }
        budget
            .consume_entries(1)
            .map_err(|source| YamlCandidateError::Budget {
                ordinal,
                document_index: self.document_index,
                source,
            })?;
        self.verify_field_guard(ordinal, &path, guard, budget)?;
        Ok(ValidatedYamlFieldGuard {
            source: self.object.source(),
            lineage: Arc::clone(&self.lineage),
            document_index: self.document_index,
            previous_ordinal: self.previous_ordinal,
            ordinal,
            path,
        })
    }

    /// Validates a replacement against a previously guarded candidate generation.
    pub(super) fn prepare_validated_replace_field(
        &mut self,
        validated: ValidatedYamlFieldGuard,
        replacement: UnityValue,
    ) -> Result<PreparedYamlFieldReplace<'_>, YamlCandidateError> {
        if !Arc::ptr_eq(&validated.lineage, &self.lineage)
            || validated.source != self.object.source()
            || validated.document_index != self.document_index
            || validated.previous_ordinal != self.previous_ordinal
        {
            return Err(YamlCandidateError::StaleFieldGuard {
                ordinal: validated.ordinal,
                document_index: self.document_index,
            });
        }
        let target = self
            .class
            .value_at_path_mut(&validated.path)
            .map_err(|source| YamlCandidateError::Path {
                ordinal: validated.ordinal,
                document_index: self.document_index,
                source,
            })?;
        Ok(PreparedYamlFieldReplace {
            target,
            previous_ordinal: &mut self.previous_ordinal,
            replacement,
            ordinal: validated.ordinal,
        })
    }

    /// Applies one operation atomically. Failed operations leave this candidate unchanged.
    pub(super) fn apply(
        &mut self,
        operation: YamlSemanticOperation<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), YamlCandidateError> {
        let ordinal = operation.ordinal();
        if let Some(previous) = self.previous_ordinal
            && ordinal <= previous
        {
            return Err(YamlCandidateError::OperationOrder {
                ordinal,
                document_index: self.document_index,
                previous,
            });
        }
        budget
            .consume_entries(1)
            .map_err(|source| YamlCandidateError::Budget {
                ordinal,
                document_index: self.document_index,
                source,
            })?;

        match operation {
            YamlSemanticOperation::FieldReplace {
                path,
                guard,
                replacement,
                ..
            } => self.replace_field(ordinal, path, guard, replacement, budget)?,
            YamlSemanticOperation::SchemaReplace {
                guard, replacement, ..
            } => self.replace_schema(ordinal, guard, replacement, budget)?,
            YamlSemanticOperation::SequenceEdit {
                path, guard, edit, ..
            } => self.edit_sequence(ordinal, path, guard, edit, budget)?,
            YamlSemanticOperation::UnsafeRaw { .. } => {
                return Err(YamlCandidateError::UnsafeRawUnsupported {
                    ordinal,
                    document_index: self.document_index,
                });
            }
        }

        self.previous_ordinal = Some(ordinal);
        Ok(())
    }

    /// Finishes the semantic candidate for encoding.
    ///
    /// This result is deliberately not a `WorkspaceObject`: only an independently reparsed final
    /// YAML artifact may produce objects exposed through `PreparedView`.
    pub(super) fn finish(self) -> FinishedYamlObject {
        FinishedYamlObject {
            document_index: self.document_index,
            class: self.class,
        }
    }

    fn replace_field(
        &mut self,
        ordinal: u32,
        path: &FieldPath,
        guard: FieldGuard,
        replacement: UnityValue,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), YamlCandidateError> {
        if path.segments().is_empty() {
            return Err(YamlCandidateError::RootFieldPath {
                ordinal,
                document_index: self.document_index,
            });
        }
        self.verify_field_guard(ordinal, path, guard, budget)?;
        let current =
            self.class
                .value_at_path_mut(path)
                .map_err(|source| YamlCandidateError::Path {
                    ordinal,
                    document_index: self.document_index,
                    source,
                })?;
        *current = replacement;
        Ok(())
    }

    fn replace_schema(
        &mut self,
        ordinal: u32,
        guard: ObjectGuard,
        replacement: UnityValue,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), YamlCandidateError> {
        self.verify_object_guard(ordinal, guard, budget)?;
        let actual = replacement.kind();
        let UnityValue::Object(properties) = replacement else {
            return Err(YamlCandidateError::ObjectReplacementType {
                ordinal,
                document_index: self.document_index,
                actual,
            });
        };
        *self.class.properties_mut() = properties;
        Ok(())
    }

    fn edit_sequence(
        &mut self,
        ordinal: u32,
        path: &FieldPath,
        guard: FieldGuard,
        edit: YamlSequenceEdit,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), YamlCandidateError> {
        if path.segments().is_empty() {
            return Err(YamlCandidateError::RootFieldPath {
                ordinal,
                document_index: self.document_index,
            });
        }
        self.verify_field_guard(ordinal, path, guard, budget)?;
        let current =
            self.class
                .value_at_path_mut(path)
                .map_err(|source| YamlCandidateError::Path {
                    ordinal,
                    document_index: self.document_index,
                    source,
                })?;
        let actual = current.kind();
        let UnityValue::Array(values) = current else {
            return Err(YamlCandidateError::SequenceTypeMismatch {
                ordinal,
                document_index: self.document_index,
                actual,
            });
        };
        apply_sequence_edit(ordinal, self.document_index, values, edit, budget)
    }

    fn verify_field_guard(
        &self,
        ordinal: u32,
        path: &FieldPath,
        guard: FieldGuard,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), YamlCandidateError> {
        let current =
            self.class
                .value_at_path(path)
                .map_err(|source| YamlCandidateError::Path {
                    ordinal,
                    document_index: self.document_index,
                    source,
                })?;
        let actual_schema = yaml_field_schema_digest(&self.class, path, current, budget)
            .map_err(|source| self.digest_error(ordinal, YamlDigestPhase::FieldSchema, source))?;
        if guard.schema_digest() != actual_schema {
            return Err(YamlCandidateError::FieldSchemaGuardMismatch {
                ordinal,
                document_index: self.document_index,
                expected: guard.schema_digest(),
                actual: actual_schema,
            });
        }
        let actual_value = semantic_value_digest(current, budget)
            .map_err(|source| self.digest_error(ordinal, YamlDigestPhase::FieldValue, source))?;
        if guard.value_digest() != actual_value {
            return Err(YamlCandidateError::FieldValueGuardMismatch {
                ordinal,
                document_index: self.document_index,
                expected: guard.value_digest(),
                actual: actual_value,
            });
        }
        Ok(())
    }

    fn verify_object_guard(
        &mut self,
        ordinal: u32,
        guard: ObjectGuard,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), YamlCandidateError> {
        let actual_schema = yaml_schema_digest(&self.class, budget)
            .map_err(|source| self.digest_error(ordinal, YamlDigestPhase::ObjectSchema, source))?;
        if guard.schema_digest() != actual_schema {
            return Err(YamlCandidateError::ObjectSchemaGuardMismatch {
                ordinal,
                document_index: self.document_index,
                expected: guard.schema_digest(),
                actual: actual_schema,
            });
        }
        let actual_value = semantic_class_value_digest(&mut self.class, budget)
            .map_err(|source| self.digest_error(ordinal, YamlDigestPhase::ObjectValue, source))?;
        if guard.value_digest() != actual_value {
            return Err(YamlCandidateError::ObjectValueGuardMismatch {
                ordinal,
                document_index: self.document_index,
                expected: guard.value_digest(),
                actual: actual_value,
            });
        }
        Ok(())
    }

    fn digest_error(
        &self,
        ordinal: u32,
        phase: YamlDigestPhase,
        source: SemanticDigestError,
    ) -> YamlCandidateError {
        YamlCandidateError::Digest {
            ordinal,
            document_index: self.document_index,
            phase,
            source: source.into(),
        }
    }
}

fn yaml_identity_matches(object: &ObjectId, document_index: usize, class: &UnityClass) -> bool {
    match (object.yaml_anchor(), object.yaml_document_ordinal()) {
        (Some(anchor), None) => anchor == class.anchor,
        (None, Some(ordinal)) => usize::try_from(ordinal) == Ok(document_index),
        (Some(_), Some(_)) | (None, None) => false,
    }
}

fn semantic_class_value_digest(
    class: &mut UnityClass,
    budget: &mut AssetLoadBudget,
) -> Result<DigestV1, SemanticDigestError> {
    let properties = take(class.properties_mut());
    let root = UnityValue::Object(properties);
    let result = semantic_value_digest(&root, budget);
    let UnityValue::Object(properties) = root else {
        unreachable!("the temporary class root is always an object")
    };
    *class.properties_mut() = properties;
    result
}

fn apply_sequence_edit(
    ordinal: u32,
    document_index: usize,
    values: &mut Vec<UnityValue>,
    edit: YamlSequenceEdit,
    budget: &mut AssetLoadBudget,
) -> Result<(), YamlCandidateError> {
    match edit {
        YamlSequenceEdit::Insert { index, value } => {
            let index = checked_sequence_index(index, values.len(), true)
                .map_err(|failure| failure.into_error(ordinal, document_index))?;
            reserve_sequence_slot(ordinal, document_index, values, budget)?;
            values.insert(index, value);
        }
        YamlSequenceEdit::Replace { index, value } => {
            let index = checked_sequence_index(index, values.len(), false)
                .map_err(|failure| failure.into_error(ordinal, document_index))?;
            values[index] = value;
        }
        YamlSequenceEdit::Remove { index } => {
            let index = checked_sequence_index(index, values.len(), false)
                .map_err(|failure| failure.into_error(ordinal, document_index))?;
            values.remove(index);
        }
        YamlSequenceEdit::Move { from, to } => {
            if from == to {
                return Err(YamlCandidateError::NoopSequenceMove {
                    ordinal,
                    document_index,
                    index: from,
                });
            }
            let from = checked_sequence_index(from, values.len(), false)
                .map_err(|failure| failure.into_error(ordinal, document_index))?;
            let to = checked_sequence_index(to, values.len(), false)
                .map_err(|failure| failure.into_error(ordinal, document_index))?;
            let value = values.remove(from);
            values.insert(to, value);
        }
        YamlSequenceEdit::Clear => values.clear(),
    }
    Ok(())
}

fn reserve_sequence_slot(
    ordinal: u32,
    document_index: usize,
    values: &mut Vec<UnityValue>,
    budget: &mut AssetLoadBudget,
) -> Result<(), YamlCandidateError> {
    budget
        .check_members(1)
        .map_err(|source| YamlCandidateError::Budget {
            ordinal,
            document_index,
            source,
        })?;
    let new_length = values
        .len()
        .checked_add(1)
        .ok_or(YamlCandidateError::ArithmeticOverflow {
            ordinal,
            document_index,
            resource: "YAML sequence length",
        })?;
    if values.len() < values.capacity() {
        budget
            .consume_members(1)
            .map_err(|source| YamlCandidateError::Budget {
                ordinal,
                document_index,
                source,
            })?;
        return Ok(());
    }

    let minimum_bytes = new_length.checked_mul(size_of::<UnityValue>()).ok_or(
        YamlCandidateError::ArithmeticOverflow {
            ordinal,
            document_index,
            resource: "YAML sequence allocation",
        },
    )?;
    let minimum_bytes =
        u64::try_from(minimum_bytes).map_err(|_| YamlCandidateError::ArithmeticOverflow {
            ordinal,
            document_index,
            resource: "YAML sequence allocation",
        })?;
    budget
        .check_bytes(minimum_bytes)
        .map_err(|source| YamlCandidateError::Budget {
            ordinal,
            document_index,
            source,
        })?;

    let mut staged = Vec::new();
    staged
        .try_reserve_exact(new_length)
        .map_err(|_| YamlCandidateError::AllocationFailed {
            ordinal,
            document_index,
            resource: "YAML sequence values",
            requested: new_length,
        })?;
    let retained_bytes = staged
        .capacity()
        .checked_mul(size_of::<UnityValue>())
        .ok_or(YamlCandidateError::ArithmeticOverflow {
            ordinal,
            document_index,
            resource: "YAML sequence allocation",
        })?;
    let retained_bytes =
        u64::try_from(retained_bytes).map_err(|_| YamlCandidateError::ArithmeticOverflow {
            ordinal,
            document_index,
            resource: "YAML sequence allocation",
        })?;
    budget
        .check_bytes(retained_bytes)
        .map_err(|source| YamlCandidateError::Budget {
            ordinal,
            document_index,
            source,
        })?;
    budget
        .consume_members(1)
        .map_err(|source| YamlCandidateError::Budget {
            ordinal,
            document_index,
            source,
        })?;
    budget
        .consume_bytes(retained_bytes)
        .map_err(|source| YamlCandidateError::Budget {
            ordinal,
            document_index,
            source,
        })?;

    staged.append(values);
    *values = staged;
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
    valid
        .then_some(converted)
        .ok_or(SequenceIndexFailure { index, length })
}

struct SequenceIndexFailure {
    index: u32,
    length: usize,
}

impl SequenceIndexFailure {
    fn into_error(self, ordinal: u32, document_index: usize) -> YamlCandidateError {
        YamlCandidateError::SequenceIndexOutOfBounds {
            ordinal,
            document_index,
            index: self.index,
            length: self.length,
        }
    }
}

/// One already-lowered YAML operation. Replacement values transfer directly into the candidate.
///
/// The lowering adapter must charge every allocation retained by an owned replacement before it
/// constructs this operation. Applying the operation itself only allocates when a sequence grows.
#[derive(Debug)]
pub(super) enum YamlSemanticOperation<'path> {
    FieldReplace {
        ordinal: u32,
        path: &'path FieldPath,
        guard: FieldGuard,
        replacement: UnityValue,
    },
    SchemaReplace {
        ordinal: u32,
        guard: ObjectGuard,
        replacement: UnityValue,
    },
    SequenceEdit {
        ordinal: u32,
        path: &'path FieldPath,
        guard: FieldGuard,
        edit: YamlSequenceEdit,
    },
    UnsafeRaw {
        ordinal: u32,
    },
}

impl YamlSemanticOperation<'_> {
    const fn ordinal(&self) -> u32 {
        match self {
            Self::FieldReplace { ordinal, .. }
            | Self::SchemaReplace { ordinal, .. }
            | Self::SequenceEdit { ordinal, .. }
            | Self::UnsafeRaw { ordinal } => *ordinal,
        }
    }
}

/// One owned sequence edit after logical references have been lowered to YAML values.
#[derive(Debug)]
pub(super) enum YamlSequenceEdit {
    Insert { index: u32, value: UnityValue },
    Replace { index: u32, value: UnityValue },
    Remove { index: u32 },
    Move { from: u32, to: u32 },
    Clear,
}

#[derive(Debug, Error)]
pub(super) enum YamlCandidateBuildError {
    #[error("workspace object does not contain a YAML value")]
    WorkspaceValueKindMismatch,
    #[error("candidate object has kind {actual:?}, expected YAML")]
    ObjectKindMismatch { actual: ObjectKind },
    #[error("candidate schema has kind {actual:?}, expected YAML")]
    SchemaKindMismatch { actual: ObjectKind },
    #[error("candidate schema class {schema} does not match YAML class {class}")]
    SchemaClassMismatch { schema: i32, class: i32 },
    #[error("candidate YAML identity does not match document {document_index}")]
    IdentityMismatch { document_index: usize },
    #[error("failed to clone the YAML candidate")]
    Clone(#[source] UnityValueCloneError),
    #[error(transparent)]
    AllocationSize(#[from] AllocationSizeError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum YamlDigestPhase {
    FieldSchema,
    FieldValue,
    ObjectSchema,
    ObjectValue,
}

impl YamlDigestPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::FieldSchema => "field schema",
            Self::FieldValue => "field value",
            Self::ObjectSchema => "object schema",
            Self::ObjectValue => "object value",
        }
    }
}

impl std::fmt::Display for YamlDigestPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(super) enum YamlDigestError {
    #[error("semantic digest length overflow")]
    LengthOverflow,
    #[error("semantic value depth {actual} exceeds maximum {maximum}")]
    ValueDepthExceeded { maximum: u32, actual: u32 },
    #[error("failed to reserve {requested} elements for {resource}")]
    AllocationFailed {
        resource: &'static str,
        requested: usize,
    },
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Digest(#[from] DigestBuildError),
}

impl From<SemanticDigestError> for YamlDigestError {
    fn from(source: SemanticDigestError) -> Self {
        match source {
            SemanticDigestError::LengthOverflow => Self::LengthOverflow,
            SemanticDigestError::ValueDepthExceeded { maximum, actual } => {
                Self::ValueDepthExceeded { maximum, actual }
            }
            SemanticDigestError::AllocationFailed {
                resource,
                requested,
                ..
            } => Self::AllocationFailed {
                resource,
                requested,
            },
            SemanticDigestError::Budget(source) => Self::Budget(source),
            SemanticDigestError::Digest(source) => Self::Digest(source),
        }
    }
}

/// Stable, allocation-free failure details for one YAML candidate operation.
#[derive(Debug, Error)]
pub(super) enum YamlCandidateError {
    #[error(
        "operation {ordinal} is not after operation {previous} for YAML document {document_index}"
    )]
    OperationOrder {
        ordinal: u32,
        document_index: usize,
        previous: u32,
    },
    #[error("operation {ordinal} uses a stale field guard for YAML document {document_index}")]
    StaleFieldGuard { ordinal: u32, document_index: usize },
    #[error(
        "operation {ordinal} uses the class root as a field path in YAML document {document_index}"
    )]
    RootFieldPath { ordinal: u32, document_index: usize },
    #[error(
        "operation {ordinal} cannot resolve a path in YAML document {document_index}: {source}"
    )]
    Path {
        ordinal: u32,
        document_index: usize,
        #[source]
        source: ValuePathError,
    },
    #[error("operation {ordinal} field-schema guard failed in YAML document {document_index}")]
    FieldSchemaGuardMismatch {
        ordinal: u32,
        document_index: usize,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error("operation {ordinal} field-value guard failed in YAML document {document_index}")]
    FieldValueGuardMismatch {
        ordinal: u32,
        document_index: usize,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error("operation {ordinal} object-schema guard failed in YAML document {document_index}")]
    ObjectSchemaGuardMismatch {
        ordinal: u32,
        document_index: usize,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error("operation {ordinal} object-value guard failed in YAML document {document_index}")]
    ObjectValueGuardMismatch {
        ordinal: u32,
        document_index: usize,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error(
        "operation {ordinal} expected an object replacement in YAML document {document_index}, found {actual}"
    )]
    ObjectReplacementType {
        ordinal: u32,
        document_index: usize,
        actual: UnityValueKind,
    },
    #[error(
        "operation {ordinal} expected a sequence in YAML document {document_index}, found {actual}"
    )]
    SequenceTypeMismatch {
        ordinal: u32,
        document_index: usize,
        actual: UnityValueKind,
    },
    #[error(
        "operation {ordinal} sequence index {index} is outside length {length} in YAML document {document_index}"
    )]
    SequenceIndexOutOfBounds {
        ordinal: u32,
        document_index: usize,
        index: u32,
        length: usize,
    },
    #[error(
        "operation {ordinal} sequence move keeps index {index} unchanged in YAML document {document_index}"
    )]
    NoopSequenceMove {
        ordinal: u32,
        document_index: usize,
        index: u32,
    },
    #[error(
        "operation {ordinal} cannot apply an unsafe raw replacement to YAML document {document_index}"
    )]
    UnsafeRawUnsupported { ordinal: u32, document_index: usize },
    #[error(
        "operation {ordinal} failed to compute the {phase} digest for YAML document {document_index}: {source}"
    )]
    Digest {
        ordinal: u32,
        document_index: usize,
        phase: YamlDigestPhase,
        #[source]
        source: YamlDigestError,
    },
    #[error(
        "operation {ordinal} failed to reserve {requested} elements for {resource} in YAML document {document_index}"
    )]
    AllocationFailed {
        ordinal: u32,
        document_index: usize,
        resource: &'static str,
        requested: usize,
    },
    #[error(
        "operation {ordinal} overflowed {resource} accounting in YAML document {document_index}"
    )]
    ArithmeticOverflow {
        ordinal: u32,
        document_index: usize,
        resource: &'static str,
    },
    #[error("operation {ordinal} exceeded the budget for YAML document {document_index}: {source}")]
    Budget {
        ordinal: u32,
        document_index: usize,
        #[source]
        source: BudgetError,
    },
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;
    use unity_asset_core::{
        AssetLoadLimits, RevisionedObjectHandle, SourceId, SourceKind, UnityDocument, WorkspaceId,
        WorkspaceRevision, semantic_value_digest,
    };

    use super::*;
    use crate::workspace::WorkspaceYamlObject;

    fn test_class() -> UnityClass {
        UnityClass::with_properties(
            1,
            "GameObject".to_owned(),
            "7".to_owned(),
            IndexMap::from([
                ("name".to_owned(), UnityValue::String("base".to_owned())),
                (
                    "items".to_owned(),
                    UnityValue::Array(vec![UnityValue::Integer(1), UnityValue::Integer(2)]),
                ),
            ]),
        )
    }

    fn test_object() -> ObjectId {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let source = SourceId::new(workspace, SourceKind::Yaml, 1).unwrap();
        ObjectId::yaml(source, "7").unwrap()
    }

    fn candidate_from(class: &UnityClass) -> YamlObjectCandidate {
        let schema_digest = yaml_schema_digest(class, &mut AssetLoadBudget::default()).unwrap();
        YamlObjectCandidate::from_class(
            test_object(),
            0,
            Arc::new(SchemaProvenance::yaml(class.class_id, schema_digest)),
            class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap()
    }

    fn field_guard(class: &UnityClass, path: &FieldPath) -> FieldGuard {
        let value = class.value_at_path(path).unwrap();
        let mut budget = AssetLoadBudget::default();
        FieldGuard::new(
            yaml_field_schema_digest(class, path, value, &mut budget).unwrap(),
            semantic_value_digest(value, &mut budget).unwrap(),
        )
    }

    fn object_guard(class: &mut UnityClass) -> ObjectGuard {
        let mut budget = AssetLoadBudget::default();
        let schema = yaml_schema_digest(class, &mut budget).unwrap();
        let value = semantic_class_value_digest(class, &mut budget).unwrap();
        ObjectGuard::new(schema, value)
    }

    #[test]
    fn workspace_object_constructor_consumes_identity_schema_and_yaml_backing() {
        let class = test_class();
        let schema_digest = yaml_schema_digest(&class, &mut AssetLoadBudget::default()).unwrap();
        let object = test_object();
        let observed_revision = WorkspaceRevision::new(DigestV1::hash_bytes(b"observed"));
        let handle =
            RevisionedObjectHandle::new(object.source().workspace(), observed_revision, object)
                .unwrap();
        let mut document = crate::YamlDocument::new();
        document.add_entry(class);
        let base = WorkspaceObject::from_shared(
            handle,
            WorkspaceObjectValue::Yaml(WorkspaceYamlObject::new(Arc::new(document), 0)),
            Arc::new(SchemaProvenance::yaml(1, schema_digest)),
        );

        let candidate =
            YamlObjectCandidate::from_workspace_object(base, &mut AssetLoadBudget::default())
                .unwrap();

        assert_eq!(candidate.object().yaml_anchor(), Some("7"));
        assert_eq!(candidate.document_index(), 0);
        assert_eq!(
            candidate.class().get("name").unwrap().as_str(),
            Some("base")
        );
    }

    #[test]
    fn consecutive_writes_read_current_candidate_and_failed_guard_does_not_mutate() {
        let mut candidate = candidate_from(&test_class());
        let path = FieldPath::root().push_field("name").unwrap();
        let stale = field_guard(candidate.class(), &path);
        let mut budget = AssetLoadBudget::default();

        candidate
            .apply(
                YamlSemanticOperation::FieldReplace {
                    ordinal: 0,
                    path: &path,
                    guard: stale,
                    replacement: UnityValue::String("first".to_owned()),
                },
                &mut budget,
            )
            .unwrap();
        let error = candidate
            .apply(
                YamlSemanticOperation::FieldReplace {
                    ordinal: 1,
                    path: &path,
                    guard: stale,
                    replacement: UnityValue::String("must-not-leak".to_owned()),
                },
                &mut budget,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            YamlCandidateError::FieldValueGuardMismatch { ordinal: 1, .. }
        ));
        assert_eq!(
            candidate.class().value_at_path(&path).unwrap(),
            &UnityValue::String("first".to_owned())
        );

        let current = field_guard(candidate.class(), &path);
        candidate
            .apply(
                YamlSemanticOperation::FieldReplace {
                    ordinal: 1,
                    path: &path,
                    guard: current,
                    replacement: UnityValue::String("second".to_owned()),
                },
                &mut budget,
            )
            .unwrap();
        assert_eq!(
            candidate.class().value_at_path(&path).unwrap(),
            &UnityValue::String("second".to_owned())
        );
    }

    #[test]
    fn prepared_field_replace_is_inert_until_infallible_commit() {
        let mut candidate = candidate_from(&test_class());
        let path = FieldPath::root().push_field("name").unwrap();
        let guard = field_guard(candidate.class(), &path);
        let mut budget = AssetLoadBudget::default();

        let validated = candidate
            .validate_replace_field_guard(4, path.clone(), guard, &mut budget)
            .unwrap();
        drop(validated);
        assert_eq!(
            candidate.class().get("name").unwrap().as_str(),
            Some("base")
        );

        let validated = candidate
            .validate_replace_field_guard(4, path.clone(), guard, &mut budget)
            .unwrap();
        let pending = candidate
            .prepare_validated_replace_field(validated, UnityValue::String("committed".to_owned()))
            .unwrap();
        pending.commit();
        assert_eq!(
            candidate.class().get("name").unwrap().as_str(),
            Some("committed")
        );

        let current_guard = field_guard(candidate.class(), &path);
        assert!(matches!(
            candidate.validate_replace_field_guard(4, path.clone(), current_guard, &mut budget),
            Err(YamlCandidateError::OperationOrder {
                ordinal: 4,
                previous: 4,
                ..
            })
        ));

        let current_guard = field_guard(candidate.class(), &path);
        let stale = candidate
            .validate_replace_field_guard(5, path.clone(), current_guard, &mut budget)
            .unwrap();
        candidate
            .apply(
                YamlSemanticOperation::FieldReplace {
                    ordinal: 5,
                    path: &path,
                    guard: current_guard,
                    replacement: UnityValue::String("intervening".to_owned()),
                },
                &mut budget,
            )
            .unwrap();
        assert!(matches!(
            candidate
                .prepare_validated_replace_field(stale, UnityValue::String("stale".to_owned()),),
            Err(YamlCandidateError::StaleFieldGuard { ordinal: 5, .. })
        ));
        assert_eq!(
            candidate.class().get("name").unwrap().as_str(),
            Some("intervening")
        );
    }

    #[test]
    fn detached_field_guard_cannot_cross_candidate_lineages() {
        let base = test_class();
        let mut first = candidate_from(&base);
        let mut second = candidate_from(&base);
        let path = FieldPath::root().push_field("name").unwrap();
        let initial = field_guard(first.class(), &path);
        let mut budget = AssetLoadBudget::default();
        first
            .apply(
                YamlSemanticOperation::FieldReplace {
                    ordinal: 0,
                    path: &path,
                    guard: initial,
                    replacement: UnityValue::String("first-lineage".to_owned()),
                },
                &mut budget,
            )
            .unwrap();
        second
            .apply(
                YamlSemanticOperation::FieldReplace {
                    ordinal: 0,
                    path: &path,
                    guard: initial,
                    replacement: UnityValue::String("second-lineage".to_owned()),
                },
                &mut budget,
            )
            .unwrap();
        let first_guard = field_guard(first.class(), &path);
        let detached = first
            .validate_replace_field_guard(1, path.clone(), first_guard, &mut budget)
            .unwrap();

        assert!(matches!(
            second.prepare_validated_replace_field(
                detached,
                UnityValue::String("must-not-cross".to_owned()),
            ),
            Err(YamlCandidateError::StaleFieldGuard { ordinal: 1, .. })
        ));
        assert_eq!(
            second.class().get("name").and_then(UnityValue::as_str),
            Some("second-lineage")
        );
    }

    #[test]
    fn sequence_edits_cover_insert_replace_move_remove_and_clear() {
        let mut candidate = candidate_from(&test_class());
        let path = FieldPath::root().push_field("items").unwrap();
        let mut budget = AssetLoadBudget::default();
        let edits = [
            (
                YamlSequenceEdit::Insert {
                    index: 1,
                    value: UnityValue::Integer(9),
                },
                &[1, 9, 2][..],
            ),
            (
                YamlSequenceEdit::Replace {
                    index: 2,
                    value: UnityValue::Integer(8),
                },
                &[1, 9, 8][..],
            ),
            (YamlSequenceEdit::Move { from: 2, to: 0 }, &[8, 1, 9][..]),
            (YamlSequenceEdit::Remove { index: 1 }, &[8, 9][..]),
            (YamlSequenceEdit::Clear, &[][..]),
        ];

        for (ordinal, (edit, expected)) in edits.into_iter().enumerate() {
            let guard = field_guard(candidate.class(), &path);
            candidate
                .apply(
                    YamlSemanticOperation::SequenceEdit {
                        ordinal: u32::try_from(ordinal).unwrap(),
                        path: &path,
                        guard,
                        edit,
                    },
                    &mut budget,
                )
                .unwrap();
            let UnityValue::Array(actual) = candidate.class().value_at_path(&path).unwrap() else {
                panic!("sequence edit must preserve an array")
            };
            assert_eq!(
                actual
                    .iter()
                    .map(|value| value.as_i64().unwrap())
                    .collect::<Vec<_>>(),
                expected
            );
        }
        assert_eq!(
            candidate.class().value_at_path(&path).unwrap(),
            &UnityValue::Array(Vec::new())
        );
        let UnityValue::Array(values) = candidate.class().value_at_path(&path).unwrap() else {
            panic!("sequence edit must preserve an array")
        };
        let retained_bytes = values.capacity() * size_of::<UnityValue>();
        assert_eq!(budget.usage().bytes, u64::try_from(retained_bytes).unwrap());
    }

    #[test]
    fn schema_replace_finishes_the_exact_class_for_independent_reparse() {
        let mut candidate = candidate_from(&test_class());
        let guard = object_guard(&mut candidate.class);
        let replacement = UnityValue::Object(IndexMap::from([(
            "replacement".to_owned(),
            UnityValue::Unsigned(u64::MAX),
        )]));
        let mut budget = AssetLoadBudget::default();
        candidate
            .apply(
                YamlSemanticOperation::SchemaReplace {
                    ordinal: 0,
                    guard,
                    replacement,
                },
                &mut budget,
            )
            .unwrap();
        let object = candidate.finish();
        assert_eq!(
            object.class().get("replacement"),
            Some(&UnityValue::Unsigned(u64::MAX))
        );
        assert_eq!(object.class().get("name"), None);
    }

    #[test]
    fn clone_and_sequence_growth_fail_before_candidate_state_changes() {
        let class = test_class();
        let schema_digest = yaml_schema_digest(&class, &mut AssetLoadBudget::default()).unwrap();
        let limits = AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        };
        let mut clone_budget = AssetLoadBudget::new(limits).unwrap();
        assert!(matches!(
            YamlObjectCandidate::from_class(
                test_object(),
                0,
                Arc::new(SchemaProvenance::yaml(class.class_id, schema_digest)),
                &class,
                &mut clone_budget,
            ),
            Err(YamlCandidateBuildError::Clone(
                UnityValueCloneError::Budget(BudgetError::Exceeded {
                    resource: "bytes",
                    ..
                })
            ))
        ));

        let mut candidate = candidate_from(&class);
        let path = FieldPath::root().push_field("items").unwrap();
        let guard = field_guard(candidate.class(), &path);
        let before = candidate.class().value_at_path(&path).unwrap().clone();
        let mut operation_budget = AssetLoadBudget::new(limits).unwrap();
        let error = candidate
            .apply(
                YamlSemanticOperation::SequenceEdit {
                    ordinal: 0,
                    path: &path,
                    guard,
                    edit: YamlSequenceEdit::Insert {
                        index: 2,
                        value: UnityValue::Integer(3),
                    },
                },
                &mut operation_budget,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            YamlCandidateError::Budget {
                source: BudgetError::Exceeded {
                    resource: "bytes",
                    ..
                },
                ..
            }
        ));
        assert_eq!(candidate.class().value_at_path(&path).unwrap(), &before);
        assert_eq!(operation_budget.usage().bytes, 0);
    }

    #[test]
    fn unsafe_raw_is_a_typed_yaml_rejection() {
        let mut candidate = candidate_from(&test_class());
        let error = candidate
            .apply(
                YamlSemanticOperation::UnsafeRaw { ordinal: 4 },
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            YamlCandidateError::UnsafeRawUnsupported {
                ordinal: 4,
                document_index: 0
            }
        ));
    }
}
