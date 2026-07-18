//! Versioned, deterministic mutation intent for one workspace revision.

mod input;
mod value;

use std::io::{self, Write};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use unity_asset_core::{
    DigestV1, DigestV1Builder, FieldPath, ObjectAddress, ObjectKind, SourceFingerprint, SourceKind,
    SourceLocator, WorkspaceRevision,
};

pub use input::MutationPlanReadError;
pub use value::{Float64Bits, MutationField, MutationValue, MutationValueRef, PlanBytes};

pub(crate) const MAX_PLAN_DEPTH: u32 = 59;
// Tagged object values add at most three wire containers per semantic level. The plan envelope
// adds the remaining three levels, so this remains a fixed bound rather than unbounded Serde
// recursion while preserving round trips for every valid semantic value.
pub(crate) const MAX_PLAN_WIRE_DEPTH: u32 = MAX_PLAN_DEPTH * 3 + 3;
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
            | Self::UnsafeRawReplace { target, .. } => target,
        }
    }

    fn validate(&self) -> Result<(), MutationPlanError> {
        match self {
            Self::FieldReplace { path, .. } => require_field_path("field_replace", path),
            Self::ReferenceReplace { path, .. } => require_field_path("reference_replace", path),
            Self::ResourceReplace { path, .. } => require_field_path("resource_replace", path),
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
        }
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
}

/// Deterministic sequence of guarded mutations against one workspace revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationPlan {
    base_revision: WorkspaceRevision,
    sources: Box<[SourceExpectation]>,
    payloads: Box<[PlanPayload]>,
    operations: Box<[MutationOperation]>,
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

        for action in &actions {
            action.validate()?;
        }
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
    let mut normalized: Vec<SourceExpectation> = Vec::new();
    normalized
        .try_reserve_exact(sources.len())
        .map_err(|error| MutationPlanError::AllocationFailed {
            resource: "source expectations",
            requested: sources.len(),
            message: error.to_string(),
        })?;
    for source in sources {
        if let Some(previous) = normalized.last()
            && previous.locator == source.locator
        {
            if previous.fingerprint != source.fingerprint {
                return Err(MutationPlanError::ConflictingSourceExpectation {
                    locator: source.locator,
                    first: previous.fingerprint,
                    second: source.fingerprint,
                });
            }
            continue;
        }
        normalized.push(source);
    }
    Ok(normalized)
}

fn normalize_payloads(
    mut payloads: Vec<PlanPayload>,
) -> Result<Vec<PlanPayload>, MutationPlanError> {
    payloads.sort_unstable_by_key(PlanPayload::digest);
    let mut normalized: Vec<PlanPayload> = Vec::new();
    normalized
        .try_reserve_exact(payloads.len())
        .map_err(|error| MutationPlanError::AllocationFailed {
            resource: "plan payloads",
            requested: payloads.len(),
            message: error.to_string(),
        })?;
    for payload in payloads {
        if let Some(previous) = normalized.last()
            && previous.digest == payload.digest
        {
            if previous.bytes != payload.bytes {
                return Err(MutationPlanError::ConflictingPayload(payload.digest));
            }
            continue;
        }
        normalized.push(payload);
    }
    Ok(normalized)
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
