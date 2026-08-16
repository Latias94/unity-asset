//! Transactional lowering for streamed-resource replacement operations.

use std::collections::TryReserveError;

use thiserror::Error;
use unity_asset_core::{
    AllocationSizeError, AssetLoadBudget, BudgetError, ContractError, DigestBuildError, DigestV1,
    DigestV1Builder, FieldPath, FieldPathSegment, SourceFingerprint, SourceId, SourceKind,
    SourceMemberId, UnityValue, UnityValueCloneError, UnityValueKind, vec_allocation_bytes,
};
#[cfg(test)]
use unity_asset_core::{
    ObjectKind, SemanticDigestError, UnityClass, ValuePathError, field_schema_digest,
    semantic_value_digest, yaml_field_schema_digest,
};
use unity_asset_write::artifact::{
    ArtifactBatch, ArtifactBatchDeclaration, ArtifactBuildFailurePhase, ArtifactHandle,
    ArtifactNameError, LogicalArtifactName, OutputSlot,
};
use unity_asset_write::object::PreparedSerializedFieldReplace;
use unity_asset_write::resources::{
    DeclaredStreamedResource, StreamedResourceAllocation, StreamedResourceError,
    StreamedResourceExtent, StreamedResourceFlags, StreamedResourcePlan, StreamedResourcePlanError,
    StreamedResourcePlanner, StreamedResourcePlannerError, StreamedResourcePreview,
};

#[cfg(test)]
use crate::schema::{SchemaOrigin, SchemaProvenance};

#[cfg(test)]
use super::super::FieldGuard;
use super::super::source_catalog::{CatalogError, SourceCatalogTransaction, SourceDescriptor};
use super::yaml::PreparedYamlFieldReplace;

const SIDECAR_MANIFEST_DOMAIN: &[u8] = b"unity-asset:resource-sidecar-manifest:v1\0";
const RESOURCE_ALIGNMENT: u32 = 1;

/// One globally ordered payload borrowed from the canonical mutation plan.
///
/// The digest and bytes must come from the same [`super::super::PlanPayload`]. That type validates
/// the binding when it is created or deserialized, so resource preflight may reuse the digest
/// without rereading the payload. Artifact preparation still independently verifies the emitted
/// extent before it can commit.
#[derive(Debug, Clone, Copy)]
pub(super) struct ResourcePayloadInput<'payload> {
    ordinal: u32,
    digest: DigestV1,
    bytes: &'payload [u8],
}

impl<'payload> ResourcePayloadInput<'payload> {
    #[must_use]
    pub(super) const fn new(ordinal: u32, digest: DigestV1, bytes: &'payload [u8]) -> Self {
        Self {
            ordinal,
            digest,
            bytes,
        }
    }

    #[must_use]
    #[cfg(test)]
    pub(super) const fn digest(self) -> DigestV1 {
        self.digest
    }
}

/// Physical ownership shape for the generated streamed-resource source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResourceSidecarLocation {
    /// An independently published companion of a root YAML or SerializedFile source.
    Companion { parent: SourceId },
    /// A new streamed-resource member contained by an archive, WebFile, or bundle.
    Contained { container: SourceId },
}

impl ResourceSidecarLocation {
    #[must_use]
    pub(super) const fn parent(self) -> SourceId {
        match self {
            Self::Companion { parent } => parent,
            Self::Contained { container } => container,
        }
    }

    #[must_use]
    pub(super) const fn publication_root(self) -> bool {
        matches!(self, Self::Companion { .. })
    }

    fn validate(self) -> Result<(), ResourceSidecarBuildError> {
        let parent = self.parent();
        let valid = match self {
            Self::Companion { .. } => {
                matches!(parent.kind(), SourceKind::Yaml | SourceKind::SerializedFile)
            }
            Self::Contained { .. } => matches!(
                parent.kind(),
                SourceKind::Archive | SourceKind::WebFile | SourceKind::AssetBundle
            ),
        };
        if valid {
            Ok(())
        } else {
            Err(ResourceSidecarBuildError::InvalidParentKind {
                location: self,
                actual: parent.kind(),
            })
        }
    }
}

/// Incremental, one-sidecar resource replacement transaction.
///
/// The constructor receives every resource payload in final global operation order. This lets it
/// derive the unique source path before any object field changes while retaining borrowed payload
/// bytes. Each successful `apply` appends exactly the next manifest payload.
#[derive(Debug)]
#[must_use = "finish the resource sidecar or its staged allocations are discarded"]
pub(super) struct ResourceSidecarBuilder<'payload> {
    location: ResourceSidecarLocation,
    logical_name: LogicalArtifactName,
    member_name: String,
    #[cfg(test)]
    manifest_digest: DigestV1,
    sidecar_identity: DigestV1,
    expected_length: u64,
    expected: Vec<ResourcePayloadInput<'payload>>,
    next_expected: usize,
    planner: StreamedResourcePlanner<'payload>,
}

/// One allocation preview bound to an exact manifest item and sidecar identity.
#[derive(Debug)]
#[must_use = "prepare a field replacement and commit this preview"]
pub(super) struct ResourceReplacePreview<'payload> {
    expected_index: usize,
    next_expected: usize,
    ordinal: u32,
    payload_digest: DigestV1,
    sidecar_identity: DigestV1,
    preview: StreamedResourcePreview,
    extent: StreamedResourceExtent<'payload>,
    allocation: StreamedResourceAllocation,
}

impl ResourceReplacePreview<'_> {
    #[must_use]
    #[cfg(test)]
    pub(super) const fn allocation(&self) -> StreamedResourceAllocation {
        self.allocation
    }
}

/// Candidate-specific token whose remaining mutation is allocation-free and infallible.
pub(super) trait PreparedResourceFieldReplace {
    fn commit(self);
}

impl PreparedResourceFieldReplace for PreparedYamlFieldReplace<'_> {
    fn commit(self) {
        PreparedYamlFieldReplace::commit(self);
    }
}

impl PreparedResourceFieldReplace for PreparedSerializedFieldReplace<'_> {
    fn commit(self) {
        PreparedSerializedFieldReplace::commit(self);
    }
}

#[cfg(test)]
struct TestUnityClassCandidate {
    class: Option<UnityClass>,
}

#[cfg(test)]
impl TestUnityClassCandidate {
    fn new(class: UnityClass) -> Self {
        Self { class: Some(class) }
    }

    fn class(&self) -> &UnityClass {
        self.class
            .as_ref()
            .expect("a test candidate owns its class outside a rebuild")
    }

    fn replace(&mut self, path: &FieldPath, replacement: UnityValue) {
        let class = self
            .class
            .take()
            .expect("a test candidate owns its class while committing");
        let (header, properties) = class.into_parts();
        let mut root = UnityValue::Object(properties);
        *root
            .value_at_path_mut(path)
            .expect("the test candidate path was validated before commit") = replacement;
        let UnityValue::Object(properties) = root else {
            unreachable!("a class property root remains an object")
        };
        self.class = Some(UnityClass::from_parts(header, properties));
    }
}

#[cfg(test)]
impl Clone for TestUnityClassCandidate {
    fn clone(&self) -> Self {
        Self::new(self.class().clone())
    }
}

#[cfg(test)]
impl std::ops::Deref for TestUnityClassCandidate {
    type Target = UnityClass;

    fn deref(&self) -> &Self::Target {
        self.class()
    }
}

#[cfg(test)]
struct PreparedUnityFieldReplace<'candidate, 'path> {
    candidate: &'candidate mut TestUnityClassCandidate,
    path: &'path FieldPath,
    replacement: UnityValue,
}

#[cfg(test)]
impl PreparedResourceFieldReplace for PreparedUnityFieldReplace<'_, '_> {
    fn commit(self) {
        self.candidate.replace(self.path, self.replacement);
    }
}

impl<'payload> ResourceSidecarBuilder<'payload> {
    /// Creates one content-addressed sidecar plan without copying payload bytes.
    ///
    /// `expected_count` is explicit so the implementation can reserve and charge the exact
    /// retained manifest capacity before consuming a streaming caller iterator.
    pub(super) fn content_addressed(
        location: ResourceSidecarLocation,
        flags: StreamedResourceFlags,
        directory: Option<&LogicalArtifactName>,
        base_name: &str,
        expected_count: usize,
        payloads: impl IntoIterator<Item = ResourcePayloadInput<'payload>>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ResourceSidecarBuildError> {
        location.validate()?;
        if expected_count == 0 {
            return Err(ResourceSidecarBuildError::EmptyManifest);
        }

        let expected_entries = u64::try_from(expected_count).map_err(|_| {
            ResourceSidecarBuildError::ArithmeticOverflow {
                resource: "resource sidecar manifest entries",
            }
        })?;
        let minimum_manifest_bytes =
            vec_allocation_bytes::<ResourcePayloadInput<'_>>(expected_count)?;
        budget.check_entries(expected_entries)?;
        budget.check_bytes(minimum_manifest_bytes)?;

        let mut expected = Vec::new();
        expected
            .try_reserve_exact(expected_count)
            .map_err(|source| ResourceSidecarBuildError::AllocationFailed {
                resource: "resource sidecar manifest",
                requested: expected_count,
                source,
            })?;
        let manifest_bytes = vec_allocation_bytes::<ResourcePayloadInput<'_>>(expected.capacity())?;
        budget.check_entries(expected_entries)?;
        budget.check_bytes(manifest_bytes)?;
        budget.consume_entries(expected_entries)?;
        budget.consume_bytes(manifest_bytes)?;
        let mut payloads = payloads.into_iter();
        let mut previous_ordinal = None;
        let mut expected_length = 0_u64;
        for index in 0..expected_count {
            let payload =
                payloads
                    .next()
                    .ok_or(ResourceSidecarBuildError::PayloadCountMismatch {
                        expected: expected_count,
                        actual: index,
                    })?;
            if let Some(previous) = previous_ordinal
                && payload.ordinal <= previous
            {
                return Err(ResourceSidecarBuildError::OperationOrder {
                    ordinal: payload.ordinal,
                    previous,
                });
            }
            let payload_length = u64::try_from(payload.bytes.len()).map_err(|_| {
                ResourceSidecarBuildError::ArithmeticOverflow {
                    resource: "resource sidecar payload length",
                }
            })?;
            expected_length = expected_length.checked_add(payload_length).ok_or(
                ResourceSidecarBuildError::ArithmeticOverflow {
                    resource: "resource sidecar content length",
                },
            )?;
            previous_ordinal = Some(payload.ordinal);
            expected.push(payload);
        }
        if payloads.next().is_some() {
            return Err(ResourceSidecarBuildError::PayloadCountMismatch {
                expected: expected_count,
                actual: expected_count.saturating_add(1),
            });
        }

        let manifest_digest = sidecar_manifest_digest(&expected)?;
        let logical_name =
            LogicalArtifactName::sidecar_with_budget(directory, base_name, manifest_digest, budget)
                .map_err(|source| match source {
                    ArtifactNameError::Budget(source) => ResourceSidecarBuildError::Budget(source),
                    source => ResourceSidecarBuildError::Name(source),
                })?;
        let member_name = budgeted_builder_string(logical_name.as_str(), budget)?;
        let sidecar_identity = DigestV1::hash_bytes(member_name.as_bytes());

        Ok(Self {
            location,
            logical_name,
            member_name,
            #[cfg(test)]
            manifest_digest,
            sidecar_identity,
            expected_length,
            expected,
            next_expected: 0,
            planner: StreamedResourcePlanner::new(flags),
        })
    }

    #[must_use]
    pub(super) const fn location(&self) -> ResourceSidecarLocation {
        self.location
    }

    #[must_use]
    pub(super) fn member_name(&self) -> &str {
        &self.member_name
    }

    #[must_use]
    #[cfg(test)]
    pub(super) const fn manifest_digest(&self) -> DigestV1 {
        self.manifest_digest
    }

    #[must_use]
    #[cfg(test)]
    pub(super) const fn extent_count(&self) -> usize {
        self.planner.extent_count()
    }

    #[must_use]
    #[cfg(test)]
    pub(super) const fn expected_count(&self) -> usize {
        self.expected.len()
    }

    /// Previews the exact next allocation without advancing the sidecar planner.
    pub(super) fn preview_next(
        &self,
        ordinal: u32,
        payload_digest: DigestV1,
    ) -> Result<ResourceReplacePreview<'payload>, ResourceReplaceError> {
        let expected = self.expected.get(self.next_expected).copied().ok_or(
            ResourceReplaceError::UnexpectedOperation {
                ordinal,
                expected_ordinal: None,
            },
        )?;
        if ordinal != expected.ordinal {
            return Err(ResourceReplaceError::UnexpectedOperation {
                ordinal,
                expected_ordinal: Some(expected.ordinal),
            });
        }
        if payload_digest != expected.digest {
            return Err(ResourceReplaceError::UnexpectedPayload {
                ordinal,
                expected: expected.digest,
                actual: payload_digest,
            });
        }
        let payload_length = u64::try_from(expected.bytes.len()).map_err(|_| {
            ResourceReplaceError::ArithmeticOverflow {
                ordinal,
                resource: "resource sidecar payload length",
            }
        })?;
        checked_stream_data_size(ordinal, payload_length)?;
        let next_expected =
            self.next_expected
                .checked_add(1)
                .ok_or(ResourceReplaceError::ArithmeticOverflow {
                    ordinal,
                    resource: "resource sidecar applied payload count",
                })?;
        let extent = StreamedResourceExtent::generated_declared(
            expected.bytes,
            expected.digest,
            RESOURCE_ALIGNMENT,
        )
        .map_err(|source| ResourceReplaceError::Extent { ordinal, source })?;
        let preview = self
            .planner
            .preview_next(&extent)
            .map_err(|source| ResourceReplaceError::Planner { ordinal, source })?;
        Ok(ResourceReplacePreview {
            expected_index: self.next_expected,
            next_expected,
            ordinal,
            payload_digest,
            sidecar_identity: self.sidecar_identity,
            allocation: preview.allocation(),
            preview,
            extent,
        })
    }

    /// Builds the complete resource-field value for a previously previewed allocation.
    pub(super) fn stage_preview(
        &self,
        preview: &ResourceReplacePreview<'_>,
        path: &FieldPath,
        current: &UnityValue,
        budget: &mut AssetLoadBudget,
    ) -> Result<UnityValue, ResourceReplaceError> {
        self.stage_preview_with_wire_path(preview, path, current, self.member_name(), budget)
    }

    /// Builds a complete field value using a target-specific Unity wire path.
    pub(super) fn stage_preview_with_wire_path(
        &self,
        preview: &ResourceReplacePreview<'_>,
        path: &FieldPath,
        current: &UnityValue,
        wire_path: &str,
        budget: &mut AssetLoadBudget,
    ) -> Result<UnityValue, ResourceReplaceError> {
        self.validate_preview(preview)?;
        stage_resource_value(
            preview.ordinal,
            path,
            current,
            wire_path,
            preview.allocation,
            budget,
        )
    }

    /// Advances the planner, then commits an already validated candidate token infallibly.
    pub(super) fn commit_prepared(
        &mut self,
        preview: ResourceReplacePreview<'payload>,
        prepared: impl PreparedResourceFieldReplace,
        budget: &mut AssetLoadBudget,
    ) -> Result<StreamedResourceAllocation, ResourceReplaceError> {
        self.validate_preview(&preview)?;
        let ordinal = preview.ordinal;
        let allocation = preview.allocation;
        let pushed = self
            .planner
            .push_previewed(preview.preview, preview.extent, budget)
            .map_err(|source| ResourceReplaceError::Planner { ordinal, source })?;
        debug_assert_eq!(pushed, allocation);

        prepared.commit();
        self.next_expected = preview.next_expected;
        Ok(pushed)
    }

    fn validate_preview(
        &self,
        preview: &ResourceReplacePreview<'_>,
    ) -> Result<(), ResourceReplaceError> {
        let expected = self.expected.get(self.next_expected).copied();
        let valid = preview.expected_index == self.next_expected
            && expected.is_some_and(|expected| {
                expected.ordinal == preview.ordinal && expected.digest == preview.payload_digest
            })
            && preview.sidecar_identity == self.sidecar_identity;
        if valid {
            Ok(())
        } else {
            Err(ResourceReplaceError::StalePreview {
                ordinal: preview.ordinal,
                expected_index: preview.expected_index,
                actual_index: self.next_expected,
            })
        }
    }

    /// Applies the next global resource operation to a caller-owned staged class.
    ///
    /// The allocation is previewed first. Guard/schema checks and every allocation needed for the
    /// complete replacement field then run against a detached field copy. The planner append is
    /// the last fallible step; only after it succeeds is the staged class updated infallibly.
    #[cfg(test)]
    fn apply(
        &mut self,
        ordinal: u32,
        payload_digest: DigestV1,
        path: &FieldPath,
        guard: FieldGuard,
        provenance: &SchemaProvenance,
        candidate: &mut TestUnityClassCandidate,
        budget: &mut AssetLoadBudget,
    ) -> Result<StreamedResourceAllocation, ResourceReplaceError> {
        let preview = self.preview_next(ordinal, payload_digest)?;
        validate_resource_guard(ordinal, path, guard, provenance, candidate.class(), budget)?;
        let current = candidate
            .value_at_path(path)
            .map_err(|source| ResourceReplaceError::Path { ordinal, source })?;
        let staged = self.stage_preview(&preview, path, current, budget)?;
        self.commit_prepared(
            preview,
            PreparedUnityFieldReplace {
                candidate,
                path,
                replacement: staged,
            },
            budget,
        )
    }

    /// Seals the exact sidecar only after every manifest operation was applied.
    pub(super) fn finish(
        self,
        budget: &mut AssetLoadBudget,
    ) -> Result<FinishedResourceSidecar<'payload>, ResourceSidecarFinishError> {
        if self.next_expected != self.expected.len() {
            return Err(ResourceSidecarFinishError::Incomplete {
                expected: self.expected.len(),
                applied: self.next_expected,
            });
        }
        let plan = self.planner.finish();
        if plan.extent_count() != self.expected.len() || plan.len() != self.expected_length {
            return Err(ResourceSidecarFinishError::LayoutMismatch {
                expected_extents: self.expected.len(),
                actual_extents: plan.extent_count(),
                expected_length: self.expected_length,
                actual_length: plan.len(),
            });
        }
        // Account for the complete content visit before reading any payload. PlanPayload already
        // proved each extent digest; this is the single aggregate scan retained by preflight.
        budget.consume_bytes(self.expected_length)?;
        let content_digest = sidecar_content_digest(&self.expected, self.expected_length)?;

        Ok(FinishedResourceSidecar {
            logical_name: self.logical_name,
            update: ResourceCatalogUpdate {
                location: self.location,
                member_name: self.member_name,
                fingerprint: SourceFingerprint::new(SourceKind::StreamedResource, content_digest),
            },
            plan,
        })
    }
}

/// Catalog mutation returned to the prepare runner together with the exact sidecar artifact.
#[derive(Debug)]
pub(super) struct ResourceCatalogUpdate {
    location: ResourceSidecarLocation,
    member_name: String,
    fingerprint: SourceFingerprint,
}

impl ResourceCatalogUpdate {
    #[must_use]
    pub(super) const fn location(&self) -> ResourceSidecarLocation {
        self.location
    }

    #[must_use]
    #[cfg(test)]
    pub(super) fn member_name(&self) -> &str {
        &self.member_name
    }

    #[must_use]
    #[cfg(test)]
    pub(super) const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }

    pub(super) fn into_descriptor(
        self,
    ) -> Result<(SourceDescriptor, SourceFingerprint), ResourceCatalogUpdateError> {
        let member = SourceMemberId::new(self.member_name)?;
        let descriptor = match self.location {
            ResourceSidecarLocation::Companion { parent } => {
                SourceDescriptor::companion(parent, member)?
            }
            ResourceSidecarLocation::Contained { container } => {
                SourceDescriptor::sidecar(container, member)?
            }
        };
        Ok((descriptor, self.fingerprint))
    }

    pub(super) fn apply(
        self,
        transaction: &mut SourceCatalogTransaction,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, ResourceCatalogUpdateError> {
        let (descriptor, fingerprint) = self.into_descriptor()?;
        Ok(transaction.register(descriptor, fingerprint, budget)?)
    }
}

/// Completed semantic sidecar retaining borrowed payload bytes but no artifact state.
pub(super) struct FinishedResourceSidecar<'payload> {
    logical_name: LogicalArtifactName,
    update: ResourceCatalogUpdate,
    plan: StreamedResourcePlan<'payload, 'payload>,
}

impl<'payload> FinishedResourceSidecar<'payload> {
    #[must_use]
    pub(super) const fn catalog_update(&self) -> &ResourceCatalogUpdate {
        &self.update
    }

    #[must_use]
    #[cfg(test)]
    pub(super) fn into_parts(
        self,
    ) -> (
        StreamedResourcePlan<'payload, 'payload>,
        ResourceCatalogUpdate,
    ) {
        (self.plan, self.update)
    }

    pub(super) fn declare(
        self,
        declaration: &mut ArtifactBatchDeclaration<'_, '_>,
    ) -> Result<DeclaredResourceSidecar<'payload>, ResourceSidecarArtifactError> {
        let Self {
            logical_name,
            update,
            plan,
        } = self;
        let plan = match update.location {
            ResourceSidecarLocation::Companion { .. } => {
                DeclaredResourcePlan::Companion(plan.declare_output(declaration, logical_name)?)
            }
            ResourceSidecarLocation::Contained { .. } => {
                drop(logical_name);
                DeclaredResourcePlan::Contained(plan)
            }
        };
        Ok(DeclaredResourceSidecar { update, plan })
    }
}

enum DeclaredResourcePlan<'payload> {
    Companion(DeclaredStreamedResource<'payload, 'payload>),
    Contained(StreamedResourcePlan<'payload, 'payload>),
}

/// Sidecar whose optional public output name has been declared before artifact encoding.
pub(super) struct DeclaredResourceSidecar<'payload> {
    update: ResourceCatalogUpdate,
    plan: DeclaredResourcePlan<'payload>,
}

impl DeclaredResourceSidecar<'_> {
    #[must_use]
    pub(super) const fn output_slot(&self) -> Option<OutputSlot> {
        match &self.plan {
            DeclaredResourcePlan::Companion(plan) => Some(plan.output_slot()),
            DeclaredResourcePlan::Contained(_) => None,
        }
    }

    pub(super) fn prepare(
        self,
        batch: &mut ArtifactBatch<'_, '_>,
    ) -> Result<PreparedResourceSidecar, ResourceSidecarArtifactError> {
        let prepared = match self.plan {
            DeclaredResourcePlan::Companion(plan) => plan.prepare(batch)?,
            DeclaredResourcePlan::Contained(plan) => plan.prepare(batch)?,
        };
        Ok(PreparedResourceSidecar {
            update: self.update,
            artifact: prepared.handle(),
        })
    }
}

/// Exact sidecar artifact plus the catalog mutation that binds it into the candidate workspace.
pub(super) struct PreparedResourceSidecar {
    update: ResourceCatalogUpdate,
    artifact: ArtifactHandle,
}

impl PreparedResourceSidecar {
    #[must_use]
    #[cfg(test)]
    pub(super) const fn artifact(&self) -> ArtifactHandle {
        self.artifact
    }

    #[must_use]
    pub(super) const fn catalog_update(&self) -> &ResourceCatalogUpdate {
        &self.update
    }

    #[must_use]
    pub(super) fn into_parts(self) -> (ArtifactHandle, ResourceCatalogUpdate) {
        (self.artifact, self.update)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceFieldVariant {
    Resource,
    StreamData,
}

impl ResourceFieldVariant {
    const fn names(self) -> ResourceFieldNames {
        match self {
            Self::Resource => ResourceFieldNames {
                source: "m_Source",
                offset: "m_Offset",
                size: "m_Size",
            },
            Self::StreamData => ResourceFieldNames {
                source: "path",
                offset: "offset",
                size: "size",
            },
        }
    }
}

#[derive(Clone, Copy)]
struct ResourceFieldNames {
    source: &'static str,
    offset: &'static str,
    size: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceIntegerKind {
    Signed,
    Unsigned,
}

#[cfg(test)]
fn validate_resource_guard(
    ordinal: u32,
    path: &FieldPath,
    guard: FieldGuard,
    provenance: &SchemaProvenance,
    candidate: &UnityClass,
    budget: &mut AssetLoadBudget,
) -> Result<(), ResourceReplaceError> {
    validate_provenance(ordinal, provenance, candidate)?;
    let current = candidate
        .value_at_path(path)
        .map_err(|source| ResourceReplaceError::Path { ordinal, source })?;
    let actual_schema = match provenance.object_kind() {
        ObjectKind::Binary => field_schema_digest(
            provenance
                .schema_digest()
                .ok_or(ResourceReplaceError::MissingSchemaDigest { ordinal })?,
            path,
        )?,
        ObjectKind::Yaml => yaml_field_schema_digest(candidate, path, current, budget)?,
    };
    if guard.schema_digest() != actual_schema {
        return Err(ResourceReplaceError::FieldSchemaGuardMismatch {
            ordinal,
            expected: guard.schema_digest(),
            actual: actual_schema,
        });
    }
    let actual_value = semantic_value_digest(current, budget)?;
    if guard.value_digest() != actual_value {
        return Err(ResourceReplaceError::FieldValueGuardMismatch {
            ordinal,
            expected: guard.value_digest(),
            actual: actual_value,
        });
    }
    Ok(())
}

fn stage_resource_value(
    ordinal: u32,
    path: &FieldPath,
    current: &UnityValue,
    wire_path: &str,
    allocation: StreamedResourceAllocation,
    budget: &mut AssetLoadBudget,
) -> Result<UnityValue, ResourceReplaceError> {
    let variant = resource_field_variant(path)
        .ok_or(ResourceReplaceError::UnsupportedFieldPath { ordinal })?;
    let names = variant.names();
    let fields = current
        .as_object()
        .ok_or(ResourceReplaceError::FieldTypeMismatch {
            ordinal,
            field: ResourceFieldPart::Container,
            actual: current.kind(),
        })?;
    let source = fields
        .get(names.source)
        .ok_or(ResourceReplaceError::MissingField {
            ordinal,
            field: ResourceFieldPart::Source,
        })?;
    if !matches!(source, UnityValue::String(_)) {
        return Err(ResourceReplaceError::FieldTypeMismatch {
            ordinal,
            field: ResourceFieldPart::Source,
            actual: source.kind(),
        });
    }
    let (offset_kind, _) = resource_integer(
        ordinal,
        ResourceFieldPart::Offset,
        fields
            .get(names.offset)
            .ok_or(ResourceReplaceError::MissingField {
                ordinal,
                field: ResourceFieldPart::Offset,
            })?,
    )?;
    let (size_kind, existing_size) = resource_integer(
        ordinal,
        ResourceFieldPart::Size,
        fields
            .get(names.size)
            .ok_or(ResourceReplaceError::MissingField {
                ordinal,
                field: ResourceFieldPart::Size,
            })?,
    )?;
    if variant == ResourceFieldVariant::StreamData && existing_size > u64::from(u32::MAX) {
        return Err(ResourceReplaceError::ExistingStreamDataSizeOverflow {
            ordinal,
            size: existing_size,
        });
    }

    let new_size = if variant == ResourceFieldVariant::StreamData {
        u64::from(checked_stream_data_size(
            ordinal,
            u64::from(allocation.size()),
        )?)
    } else {
        u64::from(allocation.size())
    };
    validate_integer_replacement(
        ordinal,
        ResourceFieldPart::Offset,
        offset_kind,
        allocation.offset(),
    )?;
    validate_integer_replacement(ordinal, ResourceFieldPart::Size, size_kind, new_size)?;

    let mut staged = current.try_clone_with_budget(budget)?;
    let new_source = budgeted_string(ordinal, wire_path, budget)?;
    let staged_fields = staged
        .as_object_mut()
        .ok_or(ResourceReplaceError::InternalStagedShape { ordinal })?;
    *staged_fields
        .get_mut(names.source)
        .ok_or(ResourceReplaceError::InternalStagedShape { ordinal })? =
        UnityValue::String(new_source);
    replace_integer(
        staged_fields
            .get_mut(names.offset)
            .ok_or(ResourceReplaceError::InternalStagedShape { ordinal })?,
        offset_kind,
        allocation.offset(),
    );
    replace_integer(
        staged_fields
            .get_mut(names.size)
            .ok_or(ResourceReplaceError::InternalStagedShape { ordinal })?,
        size_kind,
        new_size,
    );
    Ok(staged)
}

#[cfg(test)]
fn validate_provenance(
    ordinal: u32,
    provenance: &SchemaProvenance,
    candidate: &UnityClass,
) -> Result<(), ResourceReplaceError> {
    if provenance.class_id() != candidate.class_id() {
        return Err(ResourceReplaceError::SchemaClassMismatch {
            ordinal,
            expected: provenance.class_id(),
            actual: candidate.class_id(),
        });
    }
    let valid = matches!(
        (provenance.object_kind(), provenance.origin()),
        (
            ObjectKind::Binary,
            SchemaOrigin::EmbeddedTypeTree | SchemaOrigin::FrozenRegistry
        ) | (ObjectKind::Yaml, SchemaOrigin::YamlShape)
    );
    if !valid {
        return Err(ResourceReplaceError::UnsupportedSchemaProvenance {
            ordinal,
            kind: provenance.object_kind(),
            origin: provenance.origin(),
        });
    }
    Ok(())
}

fn resource_field_variant(path: &FieldPath) -> Option<ResourceFieldVariant> {
    match path.segments() {
        [FieldPathSegment::Field(name)] if name == "m_Resource" => {
            Some(ResourceFieldVariant::Resource)
        }
        [FieldPathSegment::Field(name)] if name == "m_StreamData" => {
            Some(ResourceFieldVariant::StreamData)
        }
        _ => None,
    }
}

fn resource_integer(
    ordinal: u32,
    field: ResourceFieldPart,
    value: &UnityValue,
) -> Result<(ResourceIntegerKind, u64), ResourceReplaceError> {
    match value {
        UnityValue::Integer(value) if *value >= 0 => Ok((
            ResourceIntegerKind::Signed,
            u64::try_from(*value).map_err(|_| ResourceReplaceError::NegativeInteger {
                ordinal,
                field,
                actual: *value,
            })?,
        )),
        UnityValue::Integer(value) => Err(ResourceReplaceError::NegativeInteger {
            ordinal,
            field,
            actual: *value,
        }),
        UnityValue::Unsigned(value) => Ok((ResourceIntegerKind::Unsigned, *value)),
        value => Err(ResourceReplaceError::FieldTypeMismatch {
            ordinal,
            field,
            actual: value.kind(),
        }),
    }
}

fn validate_integer_replacement(
    ordinal: u32,
    field: ResourceFieldPart,
    kind: ResourceIntegerKind,
    value: u64,
) -> Result<(), ResourceReplaceError> {
    if kind == ResourceIntegerKind::Signed && value > i64::MAX as u64 {
        return Err(ResourceReplaceError::IntegerDomainOverflow {
            ordinal,
            field,
            value,
        });
    }
    Ok(())
}

fn replace_integer(value: &mut UnityValue, kind: ResourceIntegerKind, replacement: u64) {
    *value = match kind {
        ResourceIntegerKind::Signed => UnityValue::Integer(replacement as i64),
        ResourceIntegerKind::Unsigned => UnityValue::Unsigned(replacement),
    };
}

fn checked_stream_data_size(ordinal: u32, size: u64) -> Result<u32, ResourceReplaceError> {
    u32::try_from(size).map_err(|_| ResourceReplaceError::StreamDataSizeOverflow { ordinal, size })
}

fn budgeted_string(
    ordinal: u32,
    value: &str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ResourceReplaceError> {
    let minimum =
        u64::try_from(value.len()).map_err(|_| ResourceReplaceError::ArithmeticOverflow {
            ordinal,
            resource: "resource source path",
        })?;
    budget.check_bytes(minimum)?;
    let mut copy = String::new();
    copy.try_reserve_exact(value.len()).map_err(|source| {
        ResourceReplaceError::AllocationFailed {
            ordinal,
            resource: "resource source path",
            requested: value.len(),
            source,
        }
    })?;
    let actual =
        u64::try_from(copy.capacity()).map_err(|_| ResourceReplaceError::ArithmeticOverflow {
            ordinal,
            resource: "resource source path",
        })?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    copy.push_str(value);
    Ok(copy)
}

fn budgeted_builder_string(
    value: &str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ResourceSidecarBuildError> {
    let minimum =
        u64::try_from(value.len()).map_err(|_| ResourceSidecarBuildError::ArithmeticOverflow {
            resource: "resource source path",
        })?;
    budget.check_bytes(minimum)?;
    let mut copy = String::new();
    copy.try_reserve_exact(value.len()).map_err(|source| {
        ResourceSidecarBuildError::AllocationFailed {
            resource: "resource source path",
            requested: value.len(),
            source,
        }
    })?;
    let actual = u64::try_from(copy.capacity()).map_err(|_| {
        ResourceSidecarBuildError::ArithmeticOverflow {
            resource: "resource source path",
        }
    })?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    copy.push_str(value);
    Ok(copy)
}

fn sidecar_manifest_digest(
    payloads: &[ResourcePayloadInput<'_>],
) -> Result<DigestV1, ResourceSidecarBuildError> {
    if let [payload] = payloads {
        return Ok(payload.digest);
    }
    let item_bytes = 4_u64
        .checked_add(8)
        .and_then(|bytes| bytes.checked_add(DigestV1::BYTE_LEN as u64))
        .ok_or(ResourceSidecarBuildError::ArithmeticOverflow {
            resource: "resource sidecar manifest item",
        })?;
    let payload_count = u64::try_from(payloads.len()).map_err(|_| {
        ResourceSidecarBuildError::ArithmeticOverflow {
            resource: "resource sidecar manifest count",
        }
    })?;
    let declared_length = u64::try_from(SIDECAR_MANIFEST_DOMAIN.len())
        .ok()
        .and_then(|domain| domain.checked_add(8))
        .and_then(|prefix| {
            item_bytes
                .checked_mul(payload_count)
                .and_then(|items| prefix.checked_add(items))
        })
        .ok_or(ResourceSidecarBuildError::ArithmeticOverflow {
            resource: "resource sidecar manifest digest length",
        })?;
    let mut digest = DigestV1Builder::new(declared_length);
    digest.update(SIDECAR_MANIFEST_DOMAIN)?;
    digest.update(&payload_count.to_le_bytes())?;
    for payload in payloads {
        let length = u64::try_from(payload.bytes.len()).map_err(|_| {
            ResourceSidecarBuildError::ArithmeticOverflow {
                resource: "resource sidecar payload length",
            }
        })?;
        digest.update(&payload.ordinal.to_le_bytes())?;
        digest.update(&length.to_le_bytes())?;
        digest.update(payload.digest.as_bytes())?;
    }
    Ok(digest.finalize()?)
}

fn sidecar_content_digest(
    payloads: &[ResourcePayloadInput<'_>],
    length: u64,
) -> Result<DigestV1, DigestBuildError> {
    if let [payload] = payloads {
        return Ok(payload.digest);
    }
    let mut digest = DigestV1Builder::new(length);
    for payload in payloads {
        digest.update(payload.bytes)?;
    }
    digest.finalize()
}

#[derive(Debug, Error)]
pub(super) enum ResourceSidecarBuildError {
    #[error("resource sidecar manifest must contain at least one payload")]
    EmptyManifest,
    #[error("resource sidecar {location:?} has unsupported parent kind {actual:?}")]
    InvalidParentKind {
        location: ResourceSidecarLocation,
        actual: SourceKind,
    },
    #[error("resource sidecar expected {expected} payloads, received {actual}")]
    PayloadCountMismatch { expected: usize, actual: usize },
    #[error("resource operation {ordinal} is not after operation {previous}")]
    OperationOrder { ordinal: u32, previous: u32 },
    #[error("resource sidecar arithmetic overflow for {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("failed to allocate {requested} entries for {resource}")]
    AllocationFailed {
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error(transparent)]
    AllocationSize(#[from] AllocationSizeError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Digest(#[from] DigestBuildError),
    #[error(transparent)]
    Name(#[from] ArtifactNameError),
}

/// Stable field position used by typed resource-shape diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResourceFieldPart {
    Container,
    Source,
    Offset,
    Size,
}

#[derive(Debug, Error)]
pub(super) enum ResourceReplaceError {
    #[error(
        "unexpected resource operation {ordinal}; next manifest ordinal is {expected_ordinal:?}"
    )]
    UnexpectedOperation {
        ordinal: u32,
        expected_ordinal: Option<u32>,
    },
    #[error("resource operation {ordinal} references the wrong payload digest")]
    UnexpectedPayload {
        ordinal: u32,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error(
        "resource operation {ordinal} uses stale preview index {expected_index}; current index is {actual_index}"
    )]
    StalePreview {
        ordinal: u32,
        expected_index: usize,
        actual_index: usize,
    },
    #[error("resource operation {ordinal} must target m_Resource or m_StreamData")]
    UnsupportedFieldPath { ordinal: u32 },
    #[error("resource operation {ordinal} cannot resolve its target path: {source}")]
    #[cfg(test)]
    Path {
        ordinal: u32,
        #[source]
        source: ValuePathError,
    },
    #[error("resource operation {ordinal} schema class {expected} does not match class {actual}")]
    #[cfg(test)]
    SchemaClassMismatch {
        ordinal: u32,
        expected: i32,
        actual: i32,
    },
    #[error("resource operation {ordinal} cannot use schema provenance {kind:?}/{origin:?}")]
    #[cfg(test)]
    UnsupportedSchemaProvenance {
        ordinal: u32,
        kind: ObjectKind,
        origin: SchemaOrigin,
    },
    #[error("resource operation {ordinal} has no trusted binary schema digest")]
    #[cfg(test)]
    MissingSchemaDigest { ordinal: u32 },
    #[error("resource operation {ordinal} field-schema guard failed")]
    #[cfg(test)]
    FieldSchemaGuardMismatch {
        ordinal: u32,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error("resource operation {ordinal} field-value guard failed")]
    #[cfg(test)]
    FieldValueGuardMismatch {
        ordinal: u32,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error("resource operation {ordinal} is missing its {field:?} field")]
    MissingField {
        ordinal: u32,
        field: ResourceFieldPart,
    },
    #[error("resource operation {ordinal} has {actual} in its {field:?} field")]
    FieldTypeMismatch {
        ordinal: u32,
        field: ResourceFieldPart,
        actual: UnityValueKind,
    },
    #[error("resource operation {ordinal} has negative {field:?} value {actual}")]
    NegativeInteger {
        ordinal: u32,
        field: ResourceFieldPart,
        actual: i64,
    },
    #[error("resource operation {ordinal} cannot store {value} in signed {field:?}")]
    IntegerDomainOverflow {
        ordinal: u32,
        field: ResourceFieldPart,
        value: u64,
    },
    #[error("resource operation {ordinal} observed m_StreamData.size {size} outside u32")]
    ExistingStreamDataSizeOverflow { ordinal: u32, size: u64 },
    #[error("resource operation {ordinal} cannot store size {size} in m_StreamData.size")]
    StreamDataSizeOverflow { ordinal: u32, size: u64 },
    #[error("resource operation {ordinal} produced an inconsistent staged field")]
    InternalStagedShape { ordinal: u32 },
    #[error("resource operation {ordinal} arithmetic overflow for {resource}")]
    ArithmeticOverflow {
        ordinal: u32,
        resource: &'static str,
    },
    #[error("resource operation {ordinal} failed to allocate {requested} bytes for {resource}")]
    AllocationFailed {
        ordinal: u32,
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error("resource operation {ordinal} cannot construct its extent: {source}")]
    Extent {
        ordinal: u32,
        #[source]
        source: StreamedResourcePlanError,
    },
    #[error("resource operation {ordinal} cannot commit its preview: {source}")]
    Planner {
        ordinal: u32,
        #[source]
        source: StreamedResourcePlannerError,
    },
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    #[cfg(test)]
    Digest(#[from] SemanticDigestError),
    #[error(transparent)]
    Clone(#[from] UnityValueCloneError),
}

#[derive(Debug, Error)]
pub(super) enum ResourceSidecarFinishError {
    #[error("resource sidecar applied {applied} of {expected} manifest payloads")]
    Incomplete { expected: usize, applied: usize },
    #[error(
        "resource sidecar layout mismatch: expected {expected_extents} extents/{expected_length} bytes, got {actual_extents} extents/{actual_length} bytes"
    )]
    LayoutMismatch {
        expected_extents: usize,
        actual_extents: usize,
        expected_length: u64,
        actual_length: u64,
    },
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Digest(#[from] DigestBuildError),
}

#[derive(Debug, Error)]
pub(super) enum ResourceCatalogUpdateError {
    #[error(transparent)]
    Identity(#[from] ContractError),
    #[error(transparent)]
    Catalog(Box<CatalogError>),
}

impl From<CatalogError> for ResourceCatalogUpdateError {
    fn from(source: CatalogError) -> Self {
        Self::Catalog(Box::new(source))
    }
}

#[derive(Debug, Error)]
pub(super) enum ResourceSidecarArtifactError {
    #[error(transparent)]
    Name(#[from] ArtifactNameError),
    #[error(transparent)]
    Streamed(#[from] StreamedResourceError),
}

impl ResourceSidecarArtifactError {
    pub(super) const fn failure_phase(&self) -> ArtifactBuildFailurePhase {
        match self {
            Self::Streamed(error) => error.failure_phase(),
            Self::Name(_) => ArtifactBuildFailurePhase::Encoding,
        }
    }
}

#[cfg(test)]
mod tests;
