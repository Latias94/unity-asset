//! Immutable read boundary for one fully prepared workspace candidate.

use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::mem::MaybeUninit;
use std::ops::Range;
use std::sync::Arc;

use thiserror::Error;
use unity_asset_binary::asset::{
    FileIdentifier, ObjectInfo, SerializedFile, SerializedFileInspection,
};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ContractError, DigestV1, ObjectAddress, ObjectId, ObjectKind,
    RevisionedObjectHandle, SourceFingerprint, SourceId, SourceKind, SourceLocator, UnityDocument,
    WorkspaceId, WorkspaceRevision, arc_slice_allocation_bytes, arc_value_allocation_bytes,
    vec_allocation_bytes, yaml_schema_digest,
};
use unity_asset_write::artifact::{
    ArtifactHandle, PreparedArtifact, PreparedArtifactFormat, PreparedArtifactSet,
};
use unity_asset_yaml::YamlDocument;

use crate::reference::input::PreparedReferenceOverlay;
use crate::reference::{
    ReferenceGraph, ReferenceGraphBuildOptions, ReferenceGraphError, ReferenceStore,
};
use crate::schema::SchemaProvenance;

use super::preflight::PrepareStage;
use super::snapshot::{
    WorkspaceSnapshot, consume_retained_bytes, consume_single_result, invalid_lookup,
    project_catalog_source, yaml_object_id,
};
use super::source_catalog::{LocatorResolution, SourceCatalog};
use super::source_loading::{map_yaml_error, validate_yaml_identities};
use super::view::{
    self, WorkspaceByteRange, WorkspaceError, WorkspaceLookup, WorkspaceObject,
    WorkspaceObjectValue, WorkspaceSource, WorkspaceView, validate_prepared_artifact,
};
use unity_asset_yaml::parse_budgeted_yaml_source;

/// Candidate artifact selected for one logical workspace source.
///
/// This is only a declaration. `PreparedState::new` independently proves the artifact and mints
/// the private source/object bindings exposed by a Prepared View.
#[derive(Debug, Clone)]
pub(crate) struct PreparedSourceBinding {
    source: SourceId,
    fingerprint: SourceFingerprint,
    artifact: ArtifactHandle,
    publication_root: bool,
}

impl PreparedSourceBinding {
    pub(crate) const fn new(
        source: SourceId,
        fingerprint: SourceFingerprint,
        artifact: ArtifactHandle,
    ) -> Self {
        Self {
            source,
            fingerprint,
            artifact,
            publication_root: true,
        }
    }

    pub(crate) const fn nested(mut self) -> Self {
        self.publication_root = false;
        self
    }

    pub(crate) const fn source(&self) -> SourceId {
        self.source
    }
}

#[derive(Debug)]
enum PreparedSourceProof {
    NonObject,
    Serialized,
    Yaml(Arc<YamlDocument>),
}

/// Private proof binding minted only after reparsing the exact artifact.
#[derive(Debug)]
pub(crate) struct ProvenPreparedSourceBinding {
    source: SourceId,
    fingerprint: SourceFingerprint,
    artifact: ArtifactHandle,
    publication_root: bool,
    proof: PreparedSourceProof,
}

impl ProvenPreparedSourceBinding {
    pub(crate) const fn source(&self) -> SourceId {
        self.source
    }

    pub(crate) const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }

    pub(crate) const fn artifact(&self) -> ArtifactHandle {
        self.artifact
    }

    pub(crate) const fn is_publication_root(&self) -> bool {
        self.publication_root
    }

    fn yaml_document(&self) -> Option<&Arc<YamlDocument>> {
        match &self.proof {
            PreparedSourceProof::Yaml(document) => Some(document),
            PreparedSourceProof::NonObject | PreparedSourceProof::Serialized => None,
        }
    }
}

#[derive(Debug)]
enum PreparedObjectProjection {
    Exact {
        value: WorkspaceObjectValue,
        schema: Arc<SchemaProvenance>,
    },
    BinaryPassthrough {
        exact_info: ObjectInfo,
    },
}

/// Exact object projection derived from a parser-proven artifact.
#[derive(Debug)]
struct PreparedObjectBinding {
    object: ObjectId,
    projection: PreparedObjectProjection,
    observed_revision: WorkspaceRevision,
}

impl PreparedObjectBinding {
    fn exact(object: WorkspaceObject) -> Result<Self, WorkspaceError> {
        let (handle, value, schema) = object.into_shared_parts();
        let observed_revision = handle.revision();
        let object = handle.into_object();
        if schema.object_kind() != object.kind() || value.class().class_id() != schema.class_id() {
            return Err(invalid_prepared_state(
                "exact object identity, value, and schema provenance disagree",
            ));
        }
        Ok(Self {
            object,
            projection: PreparedObjectProjection::Exact { value, schema },
            observed_revision,
        })
    }

    fn binary_passthrough(
        object: ObjectId,
        exact_info: ObjectInfo,
        observed_revision: WorkspaceRevision,
    ) -> Self {
        Self {
            object,
            projection: PreparedObjectProjection::BinaryPassthrough { exact_info },
            observed_revision,
        }
    }

    fn binary_replacement(&self) -> Option<&[u8]> {
        match &self.projection {
            PreparedObjectProjection::Exact {
                value: WorkspaceObjectValue::Binary(object),
                ..
            } => Some(object.raw_data()),
            PreparedObjectProjection::Exact {
                value: WorkspaceObjectValue::Yaml(_),
                ..
            }
            | PreparedObjectProjection::BinaryPassthrough { .. } => None,
        }
    }

    fn project_exact(
        &self,
        handle: &RevisionedObjectHandle,
        budget: &mut AssetLoadBudget,
    ) -> Result<Option<WorkspaceObject>, WorkspaceError> {
        match &self.projection {
            PreparedObjectProjection::Exact { value, schema } => {
                consume_single_result(
                    handle.retained_clone_bytes(),
                    "prepared workspace object projection",
                    budget,
                )?;
                Ok(Some(WorkspaceObject::from_shared(
                    handle.clone(),
                    value.clone(),
                    Arc::clone(schema),
                )))
            }
            PreparedObjectProjection::BinaryPassthrough { .. } => Ok(None),
        }
    }

    fn exact_info(&self) -> Option<&ObjectInfo> {
        match &self.projection {
            PreparedObjectProjection::BinaryPassthrough { exact_info } => Some(exact_info),
            PreparedObjectProjection::Exact { .. } => None,
        }
    }
}

/// Exact immutable overlay data, excluding the graph built from it.
pub(crate) struct PreparedStateCore {
    base: WorkspaceSnapshot,
    catalog: SourceCatalog,
    revision: WorkspaceRevision,
    plan_digest: DigestV1,
    artifacts: Arc<PreparedArtifactSet>,
    source_bindings: Vec<ProvenPreparedSourceBinding>,
    object_bindings: Vec<PreparedObjectBinding>,
}

impl PreparedStateCore {
    fn prove(
        base: WorkspaceSnapshot,
        catalog: SourceCatalog,
        plan_digest: DigestV1,
        artifacts: Arc<PreparedArtifactSet>,
        mut source_bindings: Vec<PreparedSourceBinding>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Arc<Self>, PreparedStateBuildError> {
        if catalog.workspace() != base.workspace_id() {
            return Err(WorkspaceError::from(ContractError::WorkspaceMismatch {
                expected: base.workspace_id(),
                actual: catalog.workspace(),
            })
            .into());
        }

        source_bindings.sort_unstable_by_key(PreparedSourceBinding::source);
        if source_bindings
            .windows(2)
            .any(|pair| pair[0].source == pair[1].source)
        {
            return Err(invalid_prepared_state("duplicate prepared source binding").into());
        }
        for binding in &source_bindings {
            if binding.source.workspace() != base.workspace_id() {
                return Err(WorkspaceError::from(ContractError::WorkspaceMismatch {
                    expected: base.workspace_id(),
                    actual: binding.source.workspace(),
                })
                .into());
            }
            let expected = catalog
                .fingerprint(binding.source)
                .map_err(WorkspaceError::from)?;
            if expected != binding.fingerprint {
                return Err(invalid_prepared_state(
                    "prepared source binding does not match the candidate catalog fingerprint",
                )
                .into());
            }
            let artifact = artifacts
                .artifact(binding.artifact)
                .map_err(|error| WorkspaceError::PreparedArtifact(Box::new(error)))?;
            validate_prepared_artifact(binding.source, binding.fingerprint, artifact)?;
        }

        let output_count = artifacts.outputs().len();
        let publication_root_count = source_bindings
            .iter()
            .filter(|binding| binding.publication_root)
            .count();
        if output_count != publication_root_count {
            return Err(invalid_prepared_state(
                "prepared artifact outputs and publication-root source bindings disagree",
            )
            .into());
        }
        let comparisons = output_count
            .checked_mul(source_bindings.len())
            .and_then(|count| u64::try_from(count).ok())
            .ok_or_else(|| {
                WorkspaceError::operation(
                    "prepared output ownership validation",
                    std::io::Error::other("prepared output comparison count overflowed"),
                )
            })?;
        budget
            .consume_members(comparisons)
            .map_err(WorkspaceError::from)?;
        for output in artifacts.outputs() {
            let owners = source_bindings
                .iter()
                .filter(|binding| binding.publication_root && binding.artifact == output.handle())
                .take(2)
                .count();
            if owners != 1 {
                return Err(invalid_prepared_state(
                    "every prepared artifact output must belong to exactly one publication root",
                )
                .into());
            }
        }

        for (source, _) in base.state().catalog().iter() {
            if !catalog.contains(source) {
                return Err(invalid_prepared_state(
                    "prepared candidates cannot remove a baseline source",
                )
                .into());
            }
        }

        for (source, _) in catalog.iter() {
            let candidate = catalog.fingerprint(source).map_err(WorkspaceError::from)?;
            let base_fingerprint = if base.state().catalog().contains(source) {
                Some(
                    base.state()
                        .catalog()
                        .fingerprint(source)
                        .map_err(WorkspaceError::from)?,
                )
            } else {
                None
            };
            if base_fingerprint != Some(candidate)
                && find_source_binding(&source_bindings, source).is_none()
            {
                return Err(invalid_prepared_state(
                    "candidate catalog changed a source without an exact prepared artifact",
                )
                .into());
            }
            if matches!(source.kind(), SourceKind::SerializedFile | SourceKind::Yaml)
                && base_fingerprint.is_none()
            {
                return Err(invalid_prepared_state(
                    "prepared candidates cannot introduce an object-bearing source without a frozen parse",
                )
                .into());
            }
        }

        let mut proven_sources = budgeted_vec(
            source_bindings.len(),
            "prepared source proof bindings",
            budget,
        )?;
        let object_binding_capacity =
            source_bindings.iter().try_fold(0_usize, |total, binding| {
                let artifact = artifacts
                    .artifact(binding.artifact)
                    .map_err(|error| WorkspaceError::PreparedArtifact(Box::new(error)))?;
                let count = match artifact.format() {
                    PreparedArtifactFormat::SerializedFile(inspection) => {
                        inspection.objects().len()
                    }
                    PreparedArtifactFormat::Yaml(inspection) => {
                        usize::try_from(inspection.documents()).map_err(|_| {
                            BudgetError::ArithmeticOverflow {
                                resource: "prepared_object_binding_count",
                            }
                        })?
                    }
                    _ => 0,
                };
                total.checked_add(count).ok_or_else(|| {
                    WorkspaceError::Budget(BudgetError::ArithmeticOverflow {
                        resource: "prepared_object_binding_count",
                    })
                })
            })?;
        let mut object_bindings = budgeted_vec(
            object_binding_capacity,
            "prepared object proof bindings",
            budget,
        )?;
        for binding in source_bindings {
            let artifact = artifacts
                .artifact(binding.artifact)
                .map_err(|error| WorkspaceError::PreparedArtifact(Box::new(error)))?;
            let proof = match (binding.source.kind(), artifact.format()) {
                (
                    SourceKind::SerializedFile,
                    PreparedArtifactFormat::SerializedFile(inspection),
                ) => {
                    prove_serialized_source(
                        &base,
                        binding.source,
                        artifact,
                        inspection,
                        &mut object_bindings,
                        budget,
                    )
                    .map_err(PreparedStateBuildError::IndependentReparse)?;
                    PreparedSourceProof::Serialized
                }
                (SourceKind::Yaml, PreparedArtifactFormat::Yaml(_)) => {
                    let document = prove_yaml_source(
                        &base,
                        binding.source,
                        artifact,
                        &mut object_bindings,
                        budget,
                    )
                    .map_err(PreparedStateBuildError::IndependentReparse)?;
                    PreparedSourceProof::Yaml(document)
                }
                (SourceKind::SerializedFile | SourceKind::Yaml, _) => {
                    return Err(invalid_prepared_state(
                        "object-bearing prepared sources require an independently parsed exact artifact",
                    )
                    .into());
                }
                (
                    SourceKind::AssetBundle
                    | SourceKind::WebFile
                    | SourceKind::Archive
                    | SourceKind::StreamedResource,
                    _,
                ) => PreparedSourceProof::NonObject,
            };
            proven_sources.push(ProvenPreparedSourceBinding {
                source: binding.source,
                fingerprint: binding.fingerprint,
                artifact: binding.artifact,
                publication_root: binding.publication_root,
                proof,
            });
        }

        object_bindings.sort_unstable_by(|left, right| left.object.cmp(&right.object));
        if object_bindings
            .windows(2)
            .any(|pair| pair[0].object == pair[1].object)
        {
            return Err(invalid_prepared_state("duplicate prepared object binding").into());
        }
        for binding in &object_bindings {
            let source = binding.object.source();
            if !catalog.contains(source) {
                return Err(invalid_prepared_state(
                    "prepared object belongs to a source absent from the candidate catalog",
                )
                .into());
            }
            if find_proven_source_binding(&proven_sources, source).is_none() {
                return Err(invalid_prepared_state(
                    "prepared object has no corresponding prepared source artifact",
                )
                .into());
            }
            if binding.observed_revision != base.revision() {
                return Err(invalid_prepared_state(
                    "prepared object was not observed from the declared baseline revision",
                )
                .into());
            }
            if let PreparedObjectProjection::Exact { value, schema } = &binding.projection
                && schema.class_id() != value.class().class_id()
            {
                return Err(invalid_prepared_state(
                    "prepared object class does not match exact schema provenance",
                )
                .into());
            }
        }

        let revision = base
            .state()
            .revision_for_catalog(&catalog)
            .map_err(WorkspaceError::from)?;
        budget
            .consume_bytes(arc_value_allocation_bytes::<Self>().map_err(|error| {
                WorkspaceError::operation("prepared state core allocation", error)
            })?)
            .map_err(WorkspaceError::from)?;
        Ok(Arc::new(Self {
            base,
            catalog,
            revision,
            plan_digest,
            artifacts,
            source_bindings: proven_sources,
            object_bindings,
        }))
    }

    pub(crate) const fn base(&self) -> &WorkspaceSnapshot {
        &self.base
    }

    pub(crate) const fn catalog(&self) -> &SourceCatalog {
        &self.catalog
    }

    pub(crate) const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    pub(crate) const fn artifacts(&self) -> &Arc<PreparedArtifactSet> {
        &self.artifacts
    }

    pub(crate) fn source_bindings(&self) -> &[ProvenPreparedSourceBinding] {
        &self.source_bindings
    }

    pub(super) fn source_binding(&self, source: SourceId) -> Option<&ProvenPreparedSourceBinding> {
        find_proven_source_binding(&self.source_bindings, source)
    }

    fn object_binding(&self, object: &ObjectId) -> Option<&PreparedObjectBinding> {
        self.object_bindings
            .binary_search_by(|candidate| candidate.object.cmp(object))
            .ok()
            .map(|index| &self.object_bindings[index])
    }
}

impl PreparedReferenceOverlay for PreparedStateCore {
    fn binary_replacement(&self, source: SourceId, path_id: i64) -> Option<&[u8]> {
        let object = ObjectId::binary(source, path_id).ok()?;
        self.object_binding(&object)
            .and_then(PreparedObjectBinding::binary_replacement)
    }

    fn binary_external<'overlay>(
        &'overlay self,
        source: SourceId,
        file: &'overlay SerializedFile,
        index: usize,
    ) -> Option<&'overlay FileIdentifier> {
        let _ = file;
        let binding = self.source_binding(source)?;
        let artifact = self.artifacts.artifact(binding.artifact).ok()?;
        let PreparedArtifactFormat::SerializedFile(inspection) = artifact.format() else {
            return None;
        };
        inspection.externals().get(index)
    }

    fn yaml_class<'overlay>(
        &'overlay self,
        source: SourceId,
        document_index: usize,
        base: &'overlay unity_asset_core::UnityClass,
    ) -> &'overlay unity_asset_core::UnityClass {
        self.source_binding(source)
            .and_then(ProvenPreparedSourceBinding::yaml_document)
            .and_then(|document| document.entries().get(document_index))
            .unwrap_or(base)
    }
}

impl fmt::Debug for PreparedStateCore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedStateCore")
            .field("workspace_id", &self.base.workspace_id())
            .field("base_revision", &self.base.revision())
            .field("revision", &self.revision)
            .field("plan_digest", &self.plan_digest)
            .field("source_binding_count", &self.source_bindings.len())
            .field("object_binding_count", &self.object_bindings.len())
            .finish_non_exhaustive()
    }
}

/// Fully proven immutable state, including the complete reference graph built from exact bytes.
pub(crate) struct PreparedState {
    core: Arc<PreparedStateCore>,
    reference_graph: ReferenceGraph,
    reference_store: Arc<ReferenceStore>,
}

#[derive(Debug, Error)]
pub(crate) enum PreparedStateBuildError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    IndependentReparse(WorkspaceError),
    #[error(transparent)]
    Reference(#[from] ReferenceGraphError),
}

impl PreparedStateBuildError {
    pub(crate) const fn prepare_stage(&self) -> PrepareStage {
        match self {
            Self::IndependentReparse(_) => PrepareStage::IndependentReparse,
            Self::Workspace(_) | Self::Reference(_) => PrepareStage::PreparedView,
        }
    }
}

impl PreparedState {
    pub(crate) fn new(
        base: WorkspaceSnapshot,
        catalog: SourceCatalog,
        plan_digest: DigestV1,
        artifacts: Arc<PreparedArtifactSet>,
        source_bindings: Vec<PreparedSourceBinding>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Arc<Self>, PreparedStateBuildError> {
        let state_bytes = arc_value_allocation_bytes::<Self>()
            .map_err(|error| WorkspaceError::operation("prepared state allocation", error))?;
        budget
            .check_bytes(state_bytes)
            .map_err(WorkspaceError::from)?;
        let reference_store = ReferenceStore::candidate(base.reference_store(), budget)?;
        let core = PreparedStateCore::prove(
            base,
            catalog,
            plan_digest,
            artifacts,
            source_bindings,
            budget,
        )?;
        let build_view = PreparedGraphView {
            core: Arc::clone(&core),
            reference_store: Arc::clone(&reference_store),
        };
        let reference_graph =
            ReferenceGraph::build(&build_view, ReferenceGraphBuildOptions::unbounded(), budget)?;
        if !reference_graph.is_complete()
            || reference_graph.workspace_id() != core.base.workspace_id()
            || reference_graph.revision() != core.revision
        {
            return Err(invalid_prepared_state(
                "prepared reference graph is incomplete or bound to another candidate",
            )
            .into());
        }
        budget
            .consume_bytes(state_bytes)
            .map_err(WorkspaceError::from)?;
        Ok(Arc::new(Self {
            core,
            reference_graph,
            reference_store,
        }))
    }

    pub(crate) fn artifacts(&self) -> &Arc<PreparedArtifactSet> {
        &self.core.artifacts
    }

    pub(crate) const fn core(&self) -> &Arc<PreparedStateCore> {
        &self.core
    }

    #[cfg(test)]
    fn catalog(&self) -> &SourceCatalog {
        &self.core.catalog
    }
}

impl fmt::Debug for PreparedState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedState")
            .field("core", &self.core)
            .field("reference_graph_coverage", self.reference_graph.coverage())
            .finish()
    }
}

#[derive(Clone)]
struct PreparedGraphView {
    core: Arc<PreparedStateCore>,
    reference_store: Arc<ReferenceStore>,
}

/// Read-your-writes view over one fully proven prepare result.
///
/// Cloning the view retains the same immutable candidate state. It never observes later commits
/// made by the owning workspace and never materializes a segmented artifact into one buffer.
#[derive(Clone)]
pub struct PreparedView {
    state: Arc<PreparedState>,
}

impl PreparedView {
    pub(crate) const fn new(state: Arc<PreparedState>) -> Self {
        Self { state }
    }

    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        self.state.core.base.workspace_id()
    }

    #[must_use]
    pub fn base_revision(&self) -> WorkspaceRevision {
        self.state.core.base.revision()
    }

    #[must_use]
    pub fn revision(&self) -> WorkspaceRevision {
        self.state.core.revision
    }

    #[must_use]
    pub fn plan_digest(&self) -> DigestV1 {
        self.state.core.plan_digest
    }

    #[must_use]
    pub fn reference_graph(&self) -> &ReferenceGraph {
        &self.state.reference_graph
    }

    #[cfg(test)]
    pub(crate) fn local_reference_cache_counts(&self) -> (usize, usize) {
        self.state.reference_store.local_entry_counts()
    }

    #[cfg(test)]
    pub(crate) const fn state(&self) -> &Arc<PreparedState> {
        &self.state
    }
}

impl fmt::Debug for PreparedView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedView")
            .field("workspace_id", &self.workspace_id())
            .field("base_revision", &self.base_revision())
            .field("revision", &self.revision())
            .field("plan_digest", &self.plan_digest())
            .finish_non_exhaustive()
    }
}

impl view::sealed::Sealed for PreparedView {
    fn reference_view_parts(&self) -> super::ReferenceViewParts<'_> {
        super::ReferenceViewParts::prepared(
            self.state.core.as_ref(),
            &self.state.reference_store,
            self.state.core.base.config().typetree,
        )
    }

    fn object_count_in_source(
        &self,
        source: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<usize, WorkspaceError> {
        prepared_object_count_in_source(self.state.core.as_ref(), source, budget)
    }

    fn object_descriptor_at_in_source(
        &self,
        source: SourceId,
        index: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<view::SourceObjectDescriptor, WorkspaceError> {
        prepared_object_descriptor_at_in_source(self.state.core.as_ref(), source, index, budget)
    }

    fn read_object_at_in_source(
        &self,
        descriptor: &view::SourceObjectDescriptor,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceObject, WorkspaceError> {
        prepared_read_object_at_in_source(self.state.core.as_ref(), descriptor, budget)
    }
}

impl view::sealed::Sealed for PreparedGraphView {
    fn reference_view_parts(&self) -> super::ReferenceViewParts<'_> {
        super::ReferenceViewParts::prepared(
            self.core.as_ref(),
            &self.reference_store,
            self.core.base.config().typetree,
        )
    }

    fn object_count_in_source(
        &self,
        source: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<usize, WorkspaceError> {
        prepared_object_count_in_source(self.core.as_ref(), source, budget)
    }

    fn object_descriptor_at_in_source(
        &self,
        source: SourceId,
        index: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<view::SourceObjectDescriptor, WorkspaceError> {
        prepared_object_descriptor_at_in_source(self.core.as_ref(), source, index, budget)
    }

    fn read_object_at_in_source(
        &self,
        descriptor: &view::SourceObjectDescriptor,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceObject, WorkspaceError> {
        prepared_read_object_at_in_source(self.core.as_ref(), descriptor, budget)
    }
}

trait PreparedCoreAccess {
    fn prepared_core(&self) -> &PreparedStateCore;
}

impl PreparedCoreAccess for PreparedView {
    fn prepared_core(&self) -> &PreparedStateCore {
        self.state.core.as_ref()
    }
}

impl PreparedCoreAccess for PreparedGraphView {
    fn prepared_core(&self) -> &PreparedStateCore {
        self.core.as_ref()
    }
}

macro_rules! impl_prepared_workspace_view {
    ($view:ty) => {
        impl WorkspaceView for $view {
            fn workspace_id(&self) -> WorkspaceId {
                self.prepared_core().base.workspace_id()
            }

            fn revision(&self) -> WorkspaceRevision {
                self.prepared_core().revision
            }

            fn sources(
                &self,
                budget: &mut AssetLoadBudget,
            ) -> Result<Vec<WorkspaceSource>, WorkspaceError> {
                prepared_sources(self.prepared_core(), budget)
            }

            fn source(
                &self,
                source: SourceId,
                budget: &mut AssetLoadBudget,
            ) -> Result<WorkspaceLookup<WorkspaceSource>, WorkspaceError> {
                prepared_source(self.prepared_core(), source, budget)
            }

            fn resolve_source(
                &self,
                locator: &SourceLocator,
                budget: &mut AssetLoadBudget,
            ) -> Result<WorkspaceLookup<WorkspaceSource>, WorkspaceError> {
                prepared_resolve_source(self.prepared_core(), locator, budget)
            }

            fn objects(
                &self,
                budget: &mut AssetLoadBudget,
            ) -> Result<Vec<RevisionedObjectHandle>, WorkspaceError> {
                prepared_objects(self.prepared_core(), budget)
            }

            fn resolve_object(
                &self,
                address: &ObjectAddress,
                budget: &mut AssetLoadBudget,
            ) -> Result<WorkspaceLookup<RevisionedObjectHandle>, WorkspaceError> {
                prepared_resolve_object(self.prepared_core(), address, budget)
            }

            fn object_address(
                &self,
                handle: &RevisionedObjectHandle,
                budget: &mut AssetLoadBudget,
            ) -> Result<ObjectAddress, WorkspaceError> {
                super::inspection::object_address_for_view(self, handle, budget)
            }

            fn read_object(
                &self,
                handle: &RevisionedObjectHandle,
                budget: &mut AssetLoadBudget,
            ) -> Result<WorkspaceObject, WorkspaceError> {
                prepared_read_object(self.prepared_core(), handle, budget)
            }

            fn source_length(&self, source: SourceId) -> Result<u64, WorkspaceError> {
                prepared_source_length(self.prepared_core(), source)
            }

            fn read_source_range(
                &self,
                source: SourceId,
                offset: u64,
                size: u64,
                budget: &mut AssetLoadBudget,
            ) -> Result<WorkspaceByteRange, WorkspaceError> {
                prepared_read_source_range(self.prepared_core(), source, offset, size, budget)
            }
        }
    };
}

impl_prepared_workspace_view!(PreparedView);
impl_prepared_workspace_view!(PreparedGraphView);

fn prepared_sources(
    state: &PreparedStateCore,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<WorkspaceSource>, WorkspaceError> {
    let count = state.catalog.len();
    let mut sources = super::snapshot::budgeted_result_vec::<WorkspaceSource>(count, budget)?;
    for (source, _) in state.catalog.iter() {
        sources.push(project_catalog_source(
            &state.catalog,
            source,
            false,
            budget,
        )?);
    }
    Ok(sources)
}

fn prepared_source(
    state: &PreparedStateCore,
    source: SourceId,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceLookup<WorkspaceSource>, WorkspaceError> {
    if source.workspace() != state.base.workspace_id() {
        return Err(ContractError::WorkspaceMismatch {
            expected: state.base.workspace_id(),
            actual: source.workspace(),
        }
        .into());
    }
    if !state.catalog.contains(source) {
        return Ok(WorkspaceLookup::Missing);
    }
    Ok(WorkspaceLookup::Resolved(project_catalog_source(
        &state.catalog,
        source,
        true,
        budget,
    )?))
}

fn prepared_resolve_source(
    state: &PreparedStateCore,
    locator: &SourceLocator,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceLookup<WorkspaceSource>, WorkspaceError> {
    match state.catalog.classify_locator(locator) {
        LocatorResolution::Resolved(source) => Ok(WorkspaceLookup::Resolved(
            project_catalog_source(&state.catalog, source, true, budget)?,
        )),
        LocatorResolution::Unloaded => Ok(WorkspaceLookup::Unloaded),
        LocatorResolution::Missing => Ok(WorkspaceLookup::Missing),
        LocatorResolution::Invalid => invalid_lookup(
            "WORKSPACE_INVALID_SOURCE_LOCATOR",
            "source locator containment does not match the loaded source hierarchy",
            budget,
        ),
    }
}

fn prepared_objects(
    state: &PreparedStateCore,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<RevisionedObjectHandle>, WorkspaceError> {
    let mut objects = WorkspaceView::objects(&state.base, budget)?;
    objects.retain(|handle| state.catalog.contains(handle.object().source()));
    for handle in &mut objects {
        *handle = handle.clone().with_revision(state.revision);
    }
    Ok(objects)
}

fn prepared_object_count_in_source(
    state: &PreparedStateCore,
    source: SourceId,
    budget: &mut AssetLoadBudget,
) -> Result<usize, WorkspaceError> {
    if !state.catalog.contains(source) {
        return Err(WorkspaceError::MissingSource(source));
    }
    super::object_count_in_source(&state.base, source, budget)
}

fn prepared_object_descriptor_at_in_source(
    state: &PreparedStateCore,
    source: SourceId,
    index: usize,
    budget: &mut AssetLoadBudget,
) -> Result<view::SourceObjectDescriptor, WorkspaceError> {
    if !state.catalog.contains(source) {
        return Err(WorkspaceError::MissingSource(source));
    }
    Ok(
        super::object_descriptor_at_in_source(&state.base, source, index, budget)?
            .with_revision(state.revision),
    )
}

fn prepared_read_object_at_in_source(
    state: &PreparedStateCore,
    descriptor: &view::SourceObjectDescriptor,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceObject, WorkspaceError> {
    let handle = descriptor.handle();
    handle.validate_context(state.base.workspace_id(), state.revision)?;
    if !state.catalog.contains(handle.object().source()) {
        return Err(WorkspaceError::MissingSource(handle.object().source()));
    }
    if let Some(binding) = state.object_binding(handle.object())
        && let Some(object) = binding.project_exact(handle, budget)?
    {
        return Ok(object);
    }

    consume_retained_bytes(
        handle.retained_clone_bytes(),
        "prepared baseline object handle",
        budget,
    )?;
    let base_descriptor = descriptor.clone().with_revision(state.base.revision());
    let object = super::read_object_at_in_source(&state.base, &base_descriptor, budget)?;
    let object = match state
        .object_binding(handle.object())
        .and_then(PreparedObjectBinding::exact_info)
    {
        Some(info) => object.with_exact_binary_info(info.clone())?,
        None => object,
    };
    Ok(object.with_revision(state.revision))
}

fn prepared_resolve_object(
    state: &PreparedStateCore,
    address: &ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceLookup<RevisionedObjectHandle>, WorkspaceError> {
    let source = match state.catalog.classify_locator(address.source_locator()) {
        LocatorResolution::Resolved(source) => source,
        LocatorResolution::Unloaded => return Ok(WorkspaceLookup::Unloaded),
        LocatorResolution::Missing => return Ok(WorkspaceLookup::Missing),
        LocatorResolution::Invalid => {
            return invalid_lookup(
                "WORKSPACE_INVALID_OBJECT_LOCATOR",
                "object locator containment does not match the loaded source hierarchy",
                budget,
            );
        }
    };
    let expected = match address.kind() {
        ObjectKind::Binary => SourceKind::SerializedFile,
        ObjectKind::Yaml => SourceKind::Yaml,
    };
    if source.kind() != expected {
        return invalid_lookup(
            "WORKSPACE_OBJECT_KIND_MISMATCH",
            "object address kind does not match the resolved source kind",
            budget,
        );
    }
    if !state.base.state().catalog().contains(source) {
        return Ok(WorkspaceLookup::Missing);
    }
    rebind_lookup_revision(
        WorkspaceView::resolve_object(&state.base, address, budget)?,
        state.revision,
    )
}

fn prepared_read_object(
    state: &PreparedStateCore,
    handle: &RevisionedObjectHandle,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceObject, WorkspaceError> {
    handle.validate_context(state.base.workspace_id(), state.revision)?;
    if !state.catalog.contains(handle.object().source()) {
        return Err(WorkspaceError::MissingSource(handle.object().source()));
    }
    if let Some(binding) = state.object_binding(handle.object())
        && let Some(object) = binding.project_exact(handle, budget)?
    {
        return Ok(object);
    }

    consume_retained_bytes(
        handle.retained_clone_bytes(),
        "prepared baseline object handle",
        budget,
    )?;
    let base_handle = handle.clone().with_revision(state.base.revision());
    let object = WorkspaceView::read_object(&state.base, &base_handle, budget)?;
    let object = match state
        .object_binding(handle.object())
        .and_then(PreparedObjectBinding::exact_info)
    {
        Some(info) => object.with_exact_binary_info(info.clone())?,
        None => object,
    };
    Ok(object.with_revision(state.revision))
}

fn prepared_read_source_range(
    state: &PreparedStateCore,
    source: SourceId,
    offset: u64,
    size: u64,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceByteRange, WorkspaceError> {
    if source.workspace() != state.base.workspace_id() {
        return Err(ContractError::WorkspaceMismatch {
            expected: state.base.workspace_id(),
            actual: source.workspace(),
        }
        .into());
    }
    if !state.catalog.contains(source) {
        return Err(WorkspaceError::MissingSource(source));
    }
    let Some(binding) = state.source_binding(source) else {
        return WorkspaceView::read_source_range(&state.base, source, offset, size, budget);
    };
    let end = offset
        .checked_add(size)
        .ok_or(WorkspaceError::RangeOverflow { offset, size })?;
    let range = WorkspaceByteRange::from_prepared(
        source,
        binding.fingerprint,
        Arc::clone(&state.artifacts),
        binding.artifact,
        Range { start: offset, end },
    )?;
    budget.consume_bytes(size)?;
    Ok(range)
}

fn prepared_source_length(
    state: &PreparedStateCore,
    source: SourceId,
) -> Result<u64, WorkspaceError> {
    if source.workspace() != state.base.workspace_id() {
        return Err(ContractError::WorkspaceMismatch {
            expected: state.base.workspace_id(),
            actual: source.workspace(),
        }
        .into());
    }
    if !state.catalog.contains(source) {
        return Err(WorkspaceError::MissingSource(source));
    }
    let Some(binding) = state.source_binding(source) else {
        return WorkspaceView::source_length(&state.base, source);
    };
    state
        .artifacts
        .artifact(binding.artifact)
        .map(PreparedArtifact::len)
        .map_err(|error| WorkspaceError::operation("prepared source length", error))
}

#[derive(Clone, Copy)]
struct ObjectTableIndex {
    path_id: i64,
    table_index: usize,
}

fn prove_serialized_source(
    base: &WorkspaceSnapshot,
    source: SourceId,
    artifact: &PreparedArtifact,
    inspection: &SerializedFileInspection,
    object_bindings: &mut Vec<PreparedObjectBinding>,
    budget: &mut AssetLoadBudget,
) -> Result<(), WorkspaceError> {
    let entry = base
        .state()
        .store()
        .get(source)
        .ok_or(WorkspaceError::MissingSource(source))?;
    let file = entry.cached_serialized().ok_or_else(|| {
        invalid_prepared_state("SerializedFile source has no immutable baseline parse")
    })?;
    if inspection.version() != file.format().version()
        || inspection.byte_order() != file.header.byte_order()
    {
        return Err(invalid_prepared_state(
            "prepared SerializedFile changed its baseline wire version or byte order",
        ));
    }
    if inspection.objects().len() != file.objects().len() {
        return Err(invalid_prepared_state(
            "prepared SerializedFile changed the baseline object identity set",
        ));
    }

    let mut baseline = budgeted_vec(
        file.objects().len(),
        "baseline SerializedFile object index",
        budget,
    )?;
    baseline.extend(
        file.objects()
            .iter()
            .enumerate()
            .map(|(table_index, info)| ObjectTableIndex {
                path_id: info.path_id(),
                table_index,
            }),
    );
    let mut exact = budgeted_vec(
        inspection.objects().len(),
        "prepared SerializedFile object index",
        budget,
    )?;
    exact.extend(
        inspection
            .objects()
            .iter()
            .enumerate()
            .map(|(table_index, info)| ObjectTableIndex {
                path_id: info.path_id(),
                table_index,
            }),
    );
    baseline.sort_unstable_by_key(|entry| entry.path_id);
    exact.sort_unstable_by_key(|entry| entry.path_id);

    for (baseline_index, exact_index) in baseline.into_iter().zip(exact) {
        if baseline_index.path_id != exact_index.path_id {
            return Err(invalid_prepared_state(
                "prepared SerializedFile changed the baseline object identity set",
            ));
        }
        let baseline_info = &file.objects()[baseline_index.table_index];
        let exact_info = &inspection.objects()[exact_index.table_index];
        if baseline_info.class_id() != exact_info.class_id() {
            return Err(invalid_prepared_state(
                "prepared SerializedFile changed an object's class identity",
            ));
        }
        let base_handle = file
            .find_object_handle(baseline_index.path_id)
            .ok_or_else(|| invalid_prepared_state("baseline object index became inconsistent"))?;
        let baseline_bytes = base_handle.raw_data()?;
        let unchanged = artifact_range_equals(
            artifact,
            exact_info.byte_start(),
            exact_info.byte_size(),
            baseline_bytes,
            budget,
        )?;
        let object = ObjectId::binary(source, exact_info.path_id())?;
        if unchanged {
            object_bindings.push(PreparedObjectBinding::binary_passthrough(
                object,
                exact_info.clone(),
                base.revision(),
            ));
            continue;
        }

        let mut reader = artifact.reader();
        reader
            .seek(SeekFrom::Start(exact_info.byte_start()))
            .map_err(|error| WorkspaceError::operation("prepared object range seek", error))?;
        let exact_object =
            base.materialize_prepared_binary_object(&object, exact_info, &mut reader, budget)?;
        object_bindings.push(PreparedObjectBinding::exact(exact_object)?);
    }
    Ok(())
}

fn artifact_range_equals(
    artifact: &PreparedArtifact,
    offset: u64,
    size: u32,
    baseline: &[u8],
    budget: &mut AssetLoadBudget,
) -> Result<bool, WorkspaceError> {
    let size = usize::try_from(size).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "prepared_object_comparison",
    })?;
    if size != baseline.len() {
        return Ok(false);
    }
    budget.consume_bytes(u64::from(u32::try_from(size).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "prepared_object_comparison",
        }
    })?))?;
    let mut reader = artifact.reader();
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|error| WorkspaceError::operation("prepared object comparison seek", error))?;
    let mut scratch = [0_u8; 16 * 1024];
    let mut compared = 0_usize;
    while compared < baseline.len() {
        let chunk_len = scratch.len().min(baseline.len() - compared);
        reader
            .read_exact(&mut scratch[..chunk_len])
            .map_err(|error| WorkspaceError::operation("prepared object comparison read", error))?;
        if scratch[..chunk_len] != baseline[compared..compared + chunk_len] {
            return Ok(false);
        }
        compared += chunk_len;
    }
    Ok(true)
}

fn prove_yaml_source(
    base: &WorkspaceSnapshot,
    source: SourceId,
    artifact: &PreparedArtifact,
    object_bindings: &mut Vec<PreparedObjectBinding>,
    budget: &mut AssetLoadBudget,
) -> Result<Arc<YamlDocument>, WorkspaceError> {
    let encoded = materialize_artifact(artifact, budget)?;
    let parsed = parse_budgeted_yaml_source(encoded, budget)
        .map_err(|error| map_yaml_error("prepared YAML artifact parsing", error))?;
    validate_yaml_identities(parsed.document(), budget)?;
    let exact = Arc::clone(parsed.document());
    let entry = base
        .state()
        .store()
        .get(source)
        .ok_or(WorkspaceError::MissingSource(source))?;
    let baseline = entry
        .cached_yaml()
        .ok_or_else(|| invalid_prepared_state("YAML source has no immutable baseline parse"))?;
    if baseline.entries().len() != exact.entries().len() {
        return Err(invalid_prepared_state(
            "prepared YAML changed the baseline object identity set",
        ));
    }
    for (document_index, (baseline_class, exact_class)) in
        baseline.entries().iter().zip(exact.entries()).enumerate()
    {
        let baseline_object = yaml_object_id(source, document_index, baseline_class)?;
        let exact_object = yaml_object_id(source, document_index, exact_class)?;
        if baseline_object != exact_object || baseline_class.class_id() != exact_class.class_id() {
            return Err(invalid_prepared_state(
                "prepared YAML changed object order, identity, or class",
            ));
        }
        let schema_digest = yaml_schema_digest(exact_class, budget)
            .map_err(|error| WorkspaceError::operation("prepared YAML schema digest", error))?;
        budget.consume_bytes(arc_value_allocation_bytes::<SchemaProvenance>().map_err(
            |error| WorkspaceError::operation("prepared YAML schema allocation", error),
        )?)?;
        let handle =
            RevisionedObjectHandle::new(base.workspace_id(), base.revision(), exact_object)?;
        let object = WorkspaceObject::from_shared(
            handle,
            WorkspaceObjectValue::Yaml(super::WorkspaceYamlObject::new(
                Arc::clone(&exact),
                document_index,
            )),
            Arc::new(SchemaProvenance::yaml(
                exact_class.class_id(),
                schema_digest,
            )),
        );
        object_bindings.push(PreparedObjectBinding::exact(object)?);
    }
    Ok(exact)
}

fn materialize_artifact(
    artifact: &PreparedArtifact,
    budget: &mut AssetLoadBudget,
) -> Result<Arc<[u8]>, WorkspaceError> {
    let length = usize::try_from(artifact.len()).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "prepared_artifact_materialization",
    })?;
    let retained = arc_slice_allocation_bytes::<u8>(length)
        .map_err(|error| WorkspaceError::operation("prepared artifact allocation", error))?;
    budget.consume_bytes(retained)?;
    let mut bytes = Arc::<[u8]>::new_uninit_slice(length);
    Arc::get_mut(&mut bytes)
        .ok_or_else(|| {
            invalid_prepared_state("new prepared artifact allocation is unexpectedly shared")
        })?
        .fill(MaybeUninit::new(0));
    // SAFETY: every element in the uniquely owned Arc slice was initialized immediately above.
    let mut bytes: Arc<[u8]> = unsafe { bytes.assume_init() };
    let writable = Arc::get_mut(&mut bytes).ok_or_else(|| {
        invalid_prepared_state("prepared artifact allocation became shared before parsing")
    })?;
    artifact
        .reader()
        .read_exact(writable)
        .map_err(|error| WorkspaceError::operation("prepared artifact materialization", error))?;
    Ok(bytes)
}

fn budgeted_vec<T>(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, WorkspaceError> {
    let entries =
        u64::try_from(capacity).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    let minimum_bytes = vec_allocation_bytes::<T>(capacity)
        .map_err(|error| WorkspaceError::operation(resource, error))?;
    budget.check_entries(entries)?;
    budget.check_bytes(minimum_bytes)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|error| WorkspaceError::Allocation {
            resource,
            requested: capacity,
            unit: super::view::WorkspaceAllocationUnit::Elements,
            message: error.to_string(),
        })?;
    let retained_bytes = vec_allocation_bytes::<T>(values.capacity())
        .map_err(|error| WorkspaceError::operation(resource, error))?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(values)
}

fn find_source_binding(
    bindings: &[PreparedSourceBinding],
    source: SourceId,
) -> Option<&PreparedSourceBinding> {
    bindings
        .binary_search_by_key(&source, PreparedSourceBinding::source)
        .ok()
        .map(|index| &bindings[index])
}

fn find_proven_source_binding(
    bindings: &[ProvenPreparedSourceBinding],
    source: SourceId,
) -> Option<&ProvenPreparedSourceBinding> {
    bindings
        .binary_search_by_key(&source, ProvenPreparedSourceBinding::source)
        .ok()
        .map(|index| &bindings[index])
}

fn rebind_lookup_revision(
    lookup: WorkspaceLookup<RevisionedObjectHandle>,
    revision: WorkspaceRevision,
) -> Result<WorkspaceLookup<RevisionedObjectHandle>, WorkspaceError> {
    Ok(match lookup {
        WorkspaceLookup::Resolved(handle) => {
            WorkspaceLookup::Resolved(handle.with_revision(revision))
        }
        WorkspaceLookup::Ambiguous { mut candidates } => {
            for handle in &mut candidates {
                *handle = handle.clone().with_revision(revision);
            }
            WorkspaceLookup::Ambiguous { candidates }
        }
        WorkspaceLookup::Unloaded => WorkspaceLookup::Unloaded,
        WorkspaceLookup::Missing => WorkspaceLookup::Missing,
        WorkspaceLookup::Invalid { diagnostic } => WorkspaceLookup::Invalid { diagnostic },
    })
}

fn invalid_prepared_state(message: &'static str) -> WorkspaceError {
    WorkspaceError::operation("prepared state validation", std::io::Error::other(message))
}

#[cfg(test)]
mod tests;
