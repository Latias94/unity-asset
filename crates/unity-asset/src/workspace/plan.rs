//! Versioned, deterministic mutation intent for one workspace revision.

mod builder;
mod input;
mod value;

use std::io::{self, Write};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use unity_asset_core::{
    DigestV1, DigestV1Builder, FieldPath, ObjectAddress, ObjectKind, SourceFingerprint, SourceKind,
    SourceLocator, WorkspaceRevision,
};

pub use builder::{MutationPlanBuilder, MutationPlanBuilderError};
pub use input::MutationPlanReadError;
pub(crate) use value::MutationValueOwned;
pub use value::{Float64Bits, MutationField, MutationValue, MutationValueRef, PlanBytes};

pub(crate) const MAX_PLAN_DEPTH: u32 = 59;
// Tagged object values add at most three wire containers per semantic level. The fixed envelope
// allowance covers a sequence edit plus a nested logical ObjectAddress, the deepest supported
// operation shape. This remains bounded while every valid in-memory value can round-trip.
pub(crate) const MAX_PLAN_WIRE_DEPTH: u32 = MAX_PLAN_DEPTH * 3 + 9;
const MUTATION_PLAN_VERSION: u8 = 1;

/// Expected identity of one source modified by a plan.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceExpectation {
    locator: SourceLocator,
    fingerprint: SourceFingerprint,
}

impl SourceExpectation {
    #[must_use]
    pub const fn new(locator: SourceLocator, fingerprint: SourceFingerprint) -> Self {
        Self {
            locator,
            fingerprint,
        }
    }

    #[must_use]
    pub const fn locator(&self) -> &SourceLocator {
        &self.locator
    }

    #[must_use]
    pub const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }
}

/// Content-addressed bytes referenced by resource and raw replacement operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPayload {
    digest: DigestV1,
    bytes: PlanBytes,
}

impl PlanPayload {
    #[must_use]
    pub fn new(bytes: impl Into<PlanBytes>) -> Self {
        let bytes = bytes.into();
        Self {
            digest: DigestV1::hash_bytes(bytes.as_slice()),
            bytes,
        }
    }

    #[must_use]
    pub const fn digest(&self) -> DigestV1 {
        self.digest
    }

    #[must_use]
    pub const fn bytes(&self) -> &PlanBytes {
        &self.bytes
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn into_bytes(self) -> PlanBytes {
        self.bytes
    }

    fn from_wire(digest: DigestV1, bytes: PlanBytes) -> Result<Self, MutationPlanError> {
        let actual = DigestV1::hash_bytes(bytes.as_slice());
        if actual != digest {
            return Err(MutationPlanError::PayloadDigestMismatch {
                declared: digest,
                actual,
            });
        }
        Ok(Self { digest, bytes })
    }
}

#[derive(Serialize)]
struct PlanPayloadRef<'a> {
    digest: DigestV1,
    bytes: &'a PlanBytes,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanPayloadWire {
    digest: DigestV1,
    bytes: PlanBytes,
}

impl Serialize for PlanPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        PlanPayloadRef {
            digest: self.digest,
            bytes: &self.bytes,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PlanPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PlanPayloadWire::deserialize(deserializer)?;
        Self::from_wire(wire.digest, wire.bytes).map_err(serde::de::Error::custom)
    }
}

/// Guard for replacing one schema-bound field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldGuard {
    schema_digest: DigestV1,
    value_digest: DigestV1,
}

impl FieldGuard {
    #[must_use]
    pub const fn new(schema_digest: DigestV1, value_digest: DigestV1) -> Self {
        Self {
            schema_digest,
            value_digest,
        }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectGuard {
    schema_digest: DigestV1,
    value_digest: DigestV1,
}

impl ObjectGuard {
    #[must_use]
    pub const fn new(schema_digest: DigestV1, value_digest: DigestV1) -> Self {
        Self {
            schema_digest,
            value_digest,
        }
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

/// Logical reference target independent of binary file IDs and YAML GUID spelling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReferenceTarget {
    Null,
    Object { address: ObjectAddress },
}

impl ReferenceTarget {
    #[must_use]
    pub const fn null() -> Self {
        Self::Null
    }

    #[must_use]
    pub const fn object(address: ObjectAddress) -> Self {
        Self::Object { address }
    }
}

/// Required acknowledgement carried by an explicitly unsafe raw replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsafeRawAcknowledgement {
    WireInvariantsAreCallersResponsibilityV1,
}

/// One inert mutation operation. Execution belongs to workspace prepare, not this contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GenericMutation {
    FieldReplace {
        target: ObjectAddress,
        path: FieldPath,
        guard: FieldGuard,
        replacement: MutationValue,
    },
    ReferenceReplace {
        target: ObjectAddress,
        path: FieldPath,
        schema_digest: DigestV1,
        expected: ReferenceTarget,
        replacement: ReferenceTarget,
    },
    SchemaReplace {
        target: ObjectAddress,
        guard: ObjectGuard,
        replacement: MutationValue,
    },
    ResourceReplace {
        target: ObjectAddress,
        path: FieldPath,
        guard: FieldGuard,
        payload: DigestV1,
    },
    SequenceEdit {
        target: ObjectAddress,
        path: FieldPath,
        guard: FieldGuard,
        edit: SequenceMutation,
    },
    UnsafeRawReplace {
        target: ObjectAddress,
        expected_raw_digest: DigestV1,
        payload: DigestV1,
        acknowledgement: UnsafeRawAcknowledgement,
    },
}

impl GenericMutation {
    #[must_use]
    pub const fn target(&self) -> &ObjectAddress {
        match self {
            Self::FieldReplace { target, .. }
            | Self::ReferenceReplace { target, .. }
            | Self::SchemaReplace { target, .. }
            | Self::ResourceReplace { target, .. }
            | Self::SequenceEdit { target, .. }
            | Self::UnsafeRawReplace { target, .. } => target,
        }
    }

    fn validate(&self) -> Result<(), MutationPlanError> {
        match self {
            Self::FieldReplace { path, .. } => require_field_path("field_replace", path),
            Self::ReferenceReplace { path, .. } => require_field_path("reference_replace", path),
            Self::ResourceReplace { path, .. } => require_field_path("resource_replace", path),
            Self::SequenceEdit { path, edit, .. } => {
                require_field_path("sequence_edit", path)?;
                edit.validate()
            }
            Self::SchemaReplace { .. } | Self::UnsafeRawReplace { .. } => Ok(()),
        }
    }

    fn payload(&self) -> Option<DigestV1> {
        match self {
            Self::ResourceReplace { payload, .. } | Self::UnsafeRawReplace { payload, .. } => {
                Some(*payload)
            }
            Self::FieldReplace { .. }
            | Self::ReferenceReplace { .. }
            | Self::SchemaReplace { .. } => None,
            Self::SequenceEdit { .. } => None,
        }
    }
}

/// One schema-aware edit to an existing ordered sequence.
///
/// Sequence edits preserve every element they do not name. Format adapters validate the guarded
/// collection and lower the edit through its observed element schema during prepare.
/// `from` identifies an element in the observed sequence; `to` is that element's final index after
/// the move completes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SequenceMutation {
    Insert {
        index: u32,
        value: MutationValue,
    },
    Replace {
        index: u32,
        value: MutationValue,
    },
    Remove {
        index: u32,
    },
    /// Moves one element so that it occupies `to` in the resulting sequence.
    Move {
        from: u32,
        to: u32,
    },
    Clear,
}

impl SequenceMutation {
    fn validate(&self) -> Result<(), MutationPlanError> {
        if let Self::Move { from, to } = self
            && from == to
        {
            return Err(MutationPlanError::NoopSequenceMove { index: *from });
        }
        Ok(())
    }
}

/// One mutation together with its stable position in the ordered plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationOperation {
    ordinal: u32,
    action: GenericMutation,
}

impl MutationOperation {
    #[must_use]
    pub const fn ordinal(&self) -> u32 {
        self.ordinal
    }

    #[must_use]
    pub const fn action(&self) -> &GenericMutation {
        &self.action
    }

    #[must_use]
    pub(crate) fn into_action(self) -> GenericMutation {
        self.action
    }
}

/// Deterministic sequence of guarded mutations against one workspace revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationPlan {
    base_revision: WorkspaceRevision,
    sources: Box<[SourceExpectation]>,
    payloads: Box<[PlanPayload]>,
    operations: Box<[MutationOperation]>,
}

type MutationPlanParts = (
    WorkspaceRevision,
    Box<[SourceExpectation]>,
    Box<[PlanPayload]>,
    Box<[MutationOperation]>,
);

/// A revision-bound group of generic mutations produced atomically by one schema recipe.
///
/// Fragments are not part of the persisted plan wire contract. They retain recipe ordering until
/// a [`MutationPlanBuilder`] assigns the final continuous operation ordinals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationPlanFragment {
    base_revision: WorkspaceRevision,
    sources: Vec<SourceExpectation>,
    payloads: Vec<PlanPayload>,
    actions: Vec<GenericMutation>,
}

impl MutationPlanFragment {
    pub(crate) fn from_recipe(
        base_revision: WorkspaceRevision,
        sources: Vec<SourceExpectation>,
        payloads: Vec<PlanPayload>,
        actions: Vec<GenericMutation>,
    ) -> Result<Self, MutationPlanError> {
        if actions.is_empty() {
            return Err(MutationPlanError::NoOperations);
        }
        validate_operation_count(actions.len())?;
        for action in &actions {
            action.validate()?;
        }
        validate_unsafe_raw_exclusivity(&actions)?;
        let sources = normalize_sources(sources)?;
        let payloads = normalize_payloads(payloads)?;
        validate_source_coverage_without_scratch(&sources, &actions)?;
        validate_payload_coverage_without_scratch(&payloads, &actions)?;
        Ok(Self {
            base_revision,
            sources,
            payloads,
            actions,
        })
    }

    #[must_use]
    pub const fn base_revision(&self) -> WorkspaceRevision {
        self.base_revision
    }

    #[must_use]
    pub fn sources(&self) -> &[SourceExpectation] {
        &self.sources
    }

    #[must_use]
    pub fn payloads(&self) -> &[PlanPayload] {
        &self.payloads
    }

    #[must_use]
    pub fn actions(&self) -> &[GenericMutation] {
        &self.actions
    }
}

impl MutationPlan {
    pub fn new(
        base_revision: WorkspaceRevision,
        sources: Vec<SourceExpectation>,
        payloads: Vec<PlanPayload>,
        actions: Vec<GenericMutation>,
    ) -> Result<Self, MutationPlanError> {
        if actions.is_empty() {
            return Err(MutationPlanError::NoOperations);
        }
        validate_operation_count(actions.len())?;

        for action in &actions {
            action.validate()?;
        }
        validate_unsafe_raw_exclusivity(&actions)?;
        let sources = normalize_sources(sources)?;
        let payloads = normalize_payloads(payloads)?;
        validate_source_coverage(&sources, &actions)?;
        validate_payload_coverage(&payloads, &actions)?;

        let mut operations = Vec::new();
        operations
            .try_reserve_exact(actions.len())
            .map_err(|error| MutationPlanError::AllocationFailed {
                resource: "mutation operations",
                requested: actions.len(),
                message: error.to_string(),
            })?;
        for (index, action) in actions.into_iter().enumerate() {
            let ordinal = u32::try_from(index)
                .map_err(|_| MutationPlanError::OperationCountOverflow { count: index + 1 })?;
            operations.push(MutationOperation { ordinal, action });
        }

        Ok(Self {
            base_revision,
            sources: sources.into_boxed_slice(),
            payloads: payloads.into_boxed_slice(),
            operations: operations.into_boxed_slice(),
        })
    }

    #[must_use]
    pub const fn base_revision(&self) -> WorkspaceRevision {
        self.base_revision
    }

    #[must_use]
    pub const fn sources(&self) -> &[SourceExpectation] {
        &self.sources
    }

    #[must_use]
    pub const fn payloads(&self) -> &[PlanPayload] {
        &self.payloads
    }

    #[must_use]
    pub const fn operations(&self) -> &[MutationOperation] {
        &self.operations
    }

    #[must_use]
    pub(crate) fn into_parts(self) -> MutationPlanParts {
        (
            self.base_revision,
            self.sources,
            self.payloads,
            self.operations,
        )
    }

    /// Returns the compact canonical JSON bytes used for persisted plan identity.
    pub fn canonical_json(&self) -> Result<Vec<u8>, MutationPlanError> {
        serde_json::to_vec(self)
            .map_err(|error| MutationPlanError::CanonicalJson(error.to_string()))
    }

    /// Writes the compact canonical JSON bytes without adding whitespace or a trailing newline.
    pub fn write_canonical_json(&self, mut writer: impl Write) -> Result<(), MutationPlanError> {
        serde_json::to_writer(&mut writer, self)
            .map_err(|error| MutationPlanError::CanonicalJson(error.to_string()))
    }

    /// Computes the plan's byte identity from its canonical JSON representation.
    pub fn digest(&self) -> Result<DigestV1, MutationPlanError> {
        let mut counter = CountingWriter::default();
        self.write_canonical_json(&mut counter)?;

        let mut builder = DigestV1Builder::new(counter.bytes);
        self.write_canonical_json(DigestWriter(&mut builder))?;
        builder
            .finalize()
            .map_err(|error| MutationPlanError::CanonicalJson(error.to_string()))
    }
}

fn validate_operation_count(count: usize) -> Result<(), MutationPlanError> {
    if count != 0 && u32::try_from(count - 1).is_err() {
        return Err(MutationPlanError::OperationCountOverflow { count });
    }
    Ok(())
}

fn require_field_path(operation: &'static str, path: &FieldPath) -> Result<(), MutationPlanError> {
    if path.segments().is_empty() {
        return Err(MutationPlanError::RootFieldPath { operation });
    }
    Ok(())
}

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let amount = u64::try_from(buffer.len())
            .map_err(|_| io::Error::other("canonical JSON length overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(amount)
            .ok_or_else(|| io::Error::other("canonical JSON length overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DigestWriter<'builder>(&'builder mut DigestV1Builder);

impl Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.update(buffer).map_err(io::Error::other)?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Serialize)]
struct MutationPlanRef<'a> {
    version: u8,
    base_revision: WorkspaceRevision,
    sources: &'a [SourceExpectation],
    payloads: &'a [PlanPayload],
    operations: &'a [MutationOperation],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationPlanWire {
    version: u8,
    base_revision: WorkspaceRevision,
    sources: Vec<SourceExpectation>,
    payloads: Vec<PlanPayload>,
    operations: Vec<MutationOperation>,
}

impl MutationPlan {
    fn from_wire(wire: MutationPlanWire) -> Result<Self, MutationPlanError> {
        if wire.version != MUTATION_PLAN_VERSION {
            return Err(MutationPlanError::UnsupportedVersion(wire.version));
        }
        for (index, operation) in wire.operations.iter().enumerate() {
            let expected = u32::try_from(index)
                .map_err(|_| MutationPlanError::OperationCountOverflow { count: index + 1 })?;
            if operation.ordinal != expected {
                return Err(MutationPlanError::NonCanonicalOrdinal {
                    index,
                    expected,
                    actual: operation.ordinal,
                });
            }
        }

        let actions = wire
            .operations
            .into_iter()
            .map(|operation| operation.action)
            .collect();
        Self::new(wire.base_revision, wire.sources, wire.payloads, actions)
    }
}

impl Serialize for MutationPlan {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        MutationPlanRef {
            version: MUTATION_PLAN_VERSION,
            base_revision: self.base_revision,
            sources: &self.sources,
            payloads: &self.payloads,
            operations: &self.operations,
        }
        .serialize(serializer)
    }
}

fn normalize_sources(
    mut sources: Vec<SourceExpectation>,
) -> Result<Vec<SourceExpectation>, MutationPlanError> {
    sources.sort_unstable_by(|left, right| left.locator.cmp(&right.locator));
    if let Some(index) = sources.windows(2).position(|pair| {
        pair[0].locator == pair[1].locator && pair[0].fingerprint != pair[1].fingerprint
    }) {
        let first = sources[index].fingerprint;
        let second = sources.remove(index + 1);
        return Err(MutationPlanError::ConflictingSourceExpectation {
            locator: second.locator,
            first,
            second: second.fingerprint,
        });
    }
    sources.dedup_by(|right, left| right.locator == left.locator);
    Ok(sources)
}

fn normalize_payloads(
    mut payloads: Vec<PlanPayload>,
) -> Result<Vec<PlanPayload>, MutationPlanError> {
    payloads.sort_unstable_by_key(PlanPayload::digest);
    for pair in payloads.windows(2) {
        if pair[0].digest == pair[1].digest && pair[0].bytes != pair[1].bytes {
            return Err(MutationPlanError::ConflictingPayload(pair[1].digest));
        }
    }
    payloads.dedup_by(|right, left| right.digest == left.digest);
    Ok(payloads)
}

fn validate_source_coverage(
    sources: &[SourceExpectation],
    actions: &[GenericMutation],
) -> Result<(), MutationPlanError> {
    let mut targets: Vec<&SourceLocator> = Vec::new();
    targets.try_reserve_exact(actions.len()).map_err(|error| {
        MutationPlanError::AllocationFailed {
            resource: "mutation source validation",
            requested: actions.len(),
            message: error.to_string(),
        }
    })?;
    targets.extend(
        actions
            .iter()
            .map(|action| action.target().source_locator()),
    );
    targets.sort_unstable();
    targets.dedup();

    for action in actions {
        validate_action_source(sources, action)?;
    }

    for source in sources {
        if targets.binary_search(&&source.locator).is_err() {
            return Err(MutationPlanError::UnusedSourceExpectation(
                source.locator.clone(),
            ));
        }
    }
    Ok(())
}

fn validate_unsafe_raw_exclusivity(actions: &[GenericMutation]) -> Result<(), MutationPlanError> {
    for (raw_index, action) in actions.iter().enumerate() {
        if !matches!(action, GenericMutation::UnsafeRawReplace { .. }) {
            continue;
        }
        if let Some((conflicting_index, _)) = actions
            .iter()
            .enumerate()
            .find(|(index, candidate)| *index != raw_index && candidate.target() == action.target())
        {
            return Err(MutationPlanError::UnsafeRawNotExclusive {
                raw_index,
                conflicting_index,
            });
        }
    }
    Ok(())
}

fn validate_source_coverage_without_scratch(
    sources: &[SourceExpectation],
    actions: &[GenericMutation],
) -> Result<(), MutationPlanError> {
    for action in actions {
        validate_action_source(sources, action)?;
    }
    for source in sources {
        if !actions
            .iter()
            .any(|action| action.target().source_locator() == &source.locator)
        {
            return Err(MutationPlanError::UnusedSourceExpectation(
                source.locator.clone(),
            ));
        }
    }
    Ok(())
}

fn validate_action_source(
    sources: &[SourceExpectation],
    action: &GenericMutation,
) -> Result<(), MutationPlanError> {
    let target = action.target();
    let locator = target.source_locator();
    let index = sources
        .binary_search_by(|source| source.locator.cmp(locator))
        .map_err(|_| MutationPlanError::MissingSourceExpectation(locator.clone()))?;
    let expected_kind = match target.kind() {
        ObjectKind::Binary => SourceKind::SerializedFile,
        ObjectKind::Yaml => SourceKind::Yaml,
    };
    let actual_kind = sources[index].fingerprint.kind();
    if actual_kind != expected_kind {
        return Err(MutationPlanError::SourceKindMismatch {
            locator: locator.clone(),
            expected: expected_kind,
            actual: actual_kind,
        });
    }
    Ok(())
}

fn validate_payload_coverage(
    payloads: &[PlanPayload],
    actions: &[GenericMutation],
) -> Result<(), MutationPlanError> {
    let mut referenced: Vec<DigestV1> = Vec::new();
    referenced
        .try_reserve_exact(actions.len())
        .map_err(|error| MutationPlanError::AllocationFailed {
            resource: "mutation payload validation",
            requested: actions.len(),
            message: error.to_string(),
        })?;
    referenced.extend(actions.iter().filter_map(GenericMutation::payload));
    referenced.sort_unstable();
    referenced.dedup();

    for digest in &referenced {
        if payloads
            .binary_search_by_key(digest, PlanPayload::digest)
            .is_err()
        {
            return Err(MutationPlanError::MissingPayload(*digest));
        }
    }
    for payload in payloads {
        if referenced.binary_search(&payload.digest).is_err() {
            return Err(MutationPlanError::UnusedPayload(payload.digest));
        }
    }
    Ok(())
}

fn validate_payload_coverage_without_scratch(
    payloads: &[PlanPayload],
    actions: &[GenericMutation],
) -> Result<(), MutationPlanError> {
    for digest in actions.iter().filter_map(GenericMutation::payload) {
        if payloads
            .binary_search_by_key(&digest, PlanPayload::digest)
            .is_err()
        {
            return Err(MutationPlanError::MissingPayload(digest));
        }
    }
    for payload in payloads {
        if !actions
            .iter()
            .any(|action| action.payload() == Some(payload.digest))
        {
            return Err(MutationPlanError::UnusedPayload(payload.digest));
        }
    }
    Ok(())
}

/// Validation failure for an inert Mutation Plan contract.
#[derive(Debug, Error)]
pub enum MutationPlanError {
    #[error("mutation plan version {0} is unsupported")]
    UnsupportedVersion(u8),
    #[error("mutation plan must contain at least one operation")]
    NoOperations,
    #[error("mutation plan contains too many operations: {count}")]
    OperationCountOverflow { count: usize },
    #[error(
        "mutation operation at index {index} has ordinal {actual}; expected consecutive ordinal {expected}"
    )]
    NonCanonicalOrdinal {
        index: usize,
        expected: u32,
        actual: u32,
    },
    #[error("source {0:?} has no expected fingerprint")]
    MissingSourceExpectation(SourceLocator),
    #[error("source expectation {0:?} is not referenced by any operation")]
    UnusedSourceExpectation(SourceLocator),
    #[error("source {locator:?} has conflicting fingerprints {first} and {second}")]
    ConflictingSourceExpectation {
        locator: SourceLocator,
        first: SourceFingerprint,
        second: SourceFingerprint,
    },
    #[error("source {locator:?} has kind {actual:?}; operation requires {expected:?}")]
    SourceKindMismatch {
        locator: SourceLocator,
        expected: SourceKind,
        actual: SourceKind,
    },
    #[error("plan payload {0} is referenced but absent")]
    MissingPayload(DigestV1),
    #[error("plan payload {0} is present but unused")]
    UnusedPayload(DigestV1),
    #[error("two plan payloads with digest {0} contain different bytes")]
    ConflictingPayload(DigestV1),
    #[error("plan payload declares digest {declared}, but its bytes hash to {actual}")]
    PayloadDigestMismatch {
        declared: DigestV1,
        actual: DigestV1,
    },
    #[error("mutation object contains duplicate field {0:?}")]
    DuplicateObjectField(String),
    #[error("mutation object field name is empty, too long, or contains NUL: {0:?}")]
    InvalidObjectFieldName(String),
    #[error("mutation value string has {actual} bytes; maximum is {maximum}")]
    ValueStringTooLong { actual: usize, maximum: usize },
    #[error("mutation value nesting depth {actual} exceeds maximum {maximum}")]
    ValueDepthExceeded { maximum: u32, actual: u32 },
    #[error("mutation value depth arithmetic overflow")]
    ValueDepthOverflow,
    #[error("sequence move at index {index} does not change collection order")]
    NoopSequenceMove { index: u32 },
    #[error(
        "unsafe raw operation at index {raw_index} conflicts with operation {conflicting_index} on the same object"
    )]
    UnsafeRawNotExclusive {
        raw_index: usize,
        conflicting_index: usize,
    },
    #[error("{operation} requires a non-root field path; use schema_replace for whole objects")]
    RootFieldPath { operation: &'static str },
    #[error("failed to allocate {resource} capacity for {requested} elements: {message}")]
    AllocationFailed {
        resource: &'static str,
        requested: usize,
        message: String,
    },
    #[error("failed to encode canonical mutation plan JSON: {0}")]
    CanonicalJson(String),
}

#[cfg(test)]
mod consumption_tests {
    use super::*;

    fn digest(label: &[u8]) -> DigestV1 {
        DigestV1::hash_bytes(label)
    }

    #[test]
    fn owned_consumption_preserves_canonical_and_semantic_order() {
        let locator = SourceLocator::path("Assets/Data/main.assets").unwrap();
        let target = ObjectAddress::binary_at(locator.clone(), 1).unwrap();
        let referenced = ObjectAddress::binary_at(locator.clone(), 2).unwrap();
        let raw_target = ObjectAddress::binary_at(locator.clone(), 3).unwrap();
        let source = SourceExpectation::new(
            locator,
            SourceFingerprint::from_bytes(SourceKind::SerializedFile, b"source"),
        );

        let nested = MutationValue::object(vec![
            MutationField::new("z_last", MutationValue::unsigned(9)).unwrap(),
            MutationField::new(
                "a_first",
                MutationValue::array(vec![
                    MutationValue::object(vec![
                        MutationField::new("z_text", MutationValue::string("kept").unwrap())
                            .unwrap(),
                        MutationField::new(
                            "a_reference",
                            MutationValue::reference(ReferenceTarget::object(referenced)),
                        )
                        .unwrap(),
                    ])
                    .unwrap(),
                    MutationValue::bytes(vec![0xde, 0xad]),
                ])
                .unwrap(),
            )
            .unwrap(),
        ])
        .unwrap();

        let resource = PlanPayload::new(vec![0x10, 0x20]);
        let raw = PlanPayload::new(vec![0x30, 0x40, 0x50]);
        let resource_digest = resource.digest();
        let raw_digest = raw.digest();
        let mut expected_payloads = vec![
            (resource_digest, vec![0x10, 0x20]),
            (raw_digest, vec![0x30, 0x40, 0x50]),
        ];
        expected_payloads.sort_unstable_by_key(|(digest, _)| *digest);

        let payloads = if resource_digest < raw_digest {
            vec![raw, resource]
        } else {
            vec![resource, raw]
        };
        let revision = WorkspaceRevision::new(digest(b"revision"));
        let plan = MutationPlan::new(
            revision,
            vec![source],
            payloads,
            vec![
                GenericMutation::SequenceEdit {
                    target: target.clone(),
                    path: FieldPath::root().push_field("m_Items").unwrap(),
                    guard: FieldGuard::new(digest(b"schema"), digest(b"values")),
                    edit: SequenceMutation::Insert {
                        index: 3,
                        value: nested,
                    },
                },
                GenericMutation::ResourceReplace {
                    target: target.clone(),
                    path: FieldPath::root().push_field("m_StreamData").unwrap(),
                    guard: FieldGuard::new(digest(b"resource schema"), digest(b"resource value")),
                    payload: resource_digest,
                },
                GenericMutation::UnsafeRawReplace {
                    target: raw_target,
                    expected_raw_digest: digest(b"raw value"),
                    payload: raw_digest,
                    acknowledgement:
                        UnsafeRawAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
                },
            ],
        )
        .unwrap();

        let canonical = plan.canonical_json().unwrap();
        assert_eq!(canonical, serde_json::to_vec(&plan).unwrap());

        let (actual_revision, sources, payloads, operations) = plan.into_parts();
        assert_eq!(actual_revision, revision);
        assert_eq!(sources.len(), 1);

        let actual_payloads = payloads
            .into_vec()
            .into_iter()
            .map(|payload| {
                let digest = payload.digest();
                (digest, payload.into_bytes().into_vec())
            })
            .collect::<Vec<_>>();
        assert_eq!(actual_payloads, expected_payloads);

        let mut operations = operations.into_vec().into_iter();
        let sequence = operations.next().unwrap();
        assert_eq!(sequence.ordinal(), 0);
        let GenericMutation::SequenceEdit { edit, .. } = sequence.into_action() else {
            panic!("expected sequence edit");
        };
        let SequenceMutation::Insert { index, value } = edit else {
            panic!("expected sequence insertion");
        };
        assert_eq!(index, 3);

        let MutationValueOwned::Object(outer_fields) = value.into_owned() else {
            panic!("expected outer object");
        };
        let mut outer_fields = outer_fields.into_iter();
        let (name, value) = outer_fields.next().unwrap().into_parts();
        assert_eq!(name, "a_first");
        let MutationValueOwned::Array(values) = value.into_owned() else {
            panic!("expected ordered array");
        };
        let mut values = values.into_iter();
        let MutationValueOwned::Object(inner_fields) = values.next().unwrap().into_owned() else {
            panic!("expected nested object");
        };
        let mut inner_fields = inner_fields.into_iter();
        let (name, value) = inner_fields.next().unwrap().into_parts();
        assert_eq!(name, "a_reference");
        let MutationValueOwned::Reference(ReferenceTarget::Object { address }) = value.into_owned()
        else {
            panic!("expected owned object reference");
        };
        assert_eq!(address.binary_path_id(), Some(2));
        let (name, value) = inner_fields.next().unwrap().into_parts();
        assert_eq!(name, "z_text");
        assert_eq!(
            value.into_owned(),
            MutationValueOwned::String("kept".into())
        );
        assert!(inner_fields.next().is_none());
        let MutationValueOwned::Bytes(bytes) = values.next().unwrap().into_owned() else {
            panic!("expected owned bytes");
        };
        assert_eq!(bytes.into_vec(), [0xde, 0xad]);
        assert!(values.next().is_none());
        let (name, value) = outer_fields.next().unwrap().into_parts();
        assert_eq!(name, "z_last");
        assert_eq!(value.into_owned(), MutationValueOwned::Unsigned(9));
        assert!(outer_fields.next().is_none());

        let resource = operations.next().unwrap();
        assert_eq!(resource.ordinal(), 1);
        assert!(matches!(
            resource.into_action(),
            GenericMutation::ResourceReplace { payload, .. } if payload == resource_digest
        ));
        let raw = operations.next().unwrap();
        assert_eq!(raw.ordinal(), 2);
        assert!(matches!(
            raw.into_action(),
            GenericMutation::UnsafeRawReplace { payload, .. } if payload == raw_digest
        ));
        assert!(operations.next().is_none());
    }

    #[test]
    fn unsafe_raw_replacement_must_be_the_only_operation_for_its_object() {
        let locator = SourceLocator::path("Assets/Data/main.assets").unwrap();
        let target = ObjectAddress::binary_at(locator.clone(), 1).unwrap();
        let payload = PlanPayload::new(vec![0x30, 0x40]);
        let payload_digest = payload.digest();
        let source = SourceExpectation::new(
            locator,
            SourceFingerprint::from_bytes(SourceKind::SerializedFile, b"source"),
        );
        let revision = WorkspaceRevision::new(digest(b"revision"));

        let error = MutationPlan::new(
            revision,
            vec![source],
            vec![payload],
            vec![
                GenericMutation::UnsafeRawReplace {
                    target: target.clone(),
                    expected_raw_digest: digest(b"raw value"),
                    payload: payload_digest,
                    acknowledgement:
                        UnsafeRawAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
                },
                GenericMutation::FieldReplace {
                    target,
                    path: FieldPath::root().push_field("m_Name").unwrap(),
                    guard: FieldGuard::new(digest(b"schema"), digest(b"value")),
                    replacement: MutationValue::string("replacement").unwrap(),
                },
            ],
        )
        .unwrap_err();

        assert!(matches!(
            error,
            MutationPlanError::UnsafeRawNotExclusive {
                raw_index: 0,
                conflicting_index: 1,
            }
        ));
    }
}
