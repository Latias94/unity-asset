use std::fmt::Write as _;
use std::ops::Deref;

use serde::ser::SerializeSeq;
use serde::{Serialize, Serializer};
#[cfg(any(test, not(feature = "decode")))]
use unity_asset_binary::asset::class_ids;
use unity_asset_core::{
    AssetLoadBudget, ObjectAddress, ObjectKind, RevisionedObjectHandle, SourceLocator, UnityValue,
    YamlFileId,
};

use super::container::{
    BundleContainerQuery, BundleContainerResult, query_bundle_container_occurrences,
    resolved_addresses,
};
use super::contract::ExtractionAllocationUnit;
#[cfg(not(feature = "decode"))]
use super::contract::{ExtractionArtifactKind, ExtractionDiagnosticCode};
use super::manifest::canonical_digest;
use super::model::{
    ExtractionModelError, ExtractionPath, ExtractionPlan, ExtractionRepresentationPolicy,
    ExtractionRequest, ExtractionSelection, ExtractionSelectionWitness, PlannedArtifact,
};
use super::planning_contract::{
    ExtractionPlanError, ExtractionPlanMismatchKind, budgeted_vec, clone_string,
    resolve_required_handle, source_for_id, usize_to_u64,
};
#[cfg(not(feature = "decode"))]
use super::representation::PlannedContent;
use super::representation::RepresentationPlanner;
use crate::reference::{ReferenceGraph, ReferenceGraphBuildOptions};
#[cfg(test)]
use crate::workspace::StreamedResourceResolver;
use crate::workspace::{WorkspaceObject, WorkspaceSource, WorkspaceView};

/// Plans deterministic extraction artifacts against one immutable workspace view.
pub struct ExtractionPlanner<'view> {
    view: &'view dyn WorkspaceView,
}

/// Non-serializable evidence that one persisted plan matches this workspace.
pub(in crate::extraction) struct VerifiedExecutionPlan<'plan> {
    plan: &'plan ExtractionPlan,
}

impl Deref for VerifiedExecutionPlan<'_> {
    type Target = ExtractionPlan;

    fn deref(&self) -> &Self::Target {
        self.plan
    }
}

impl<'view> ExtractionPlanner<'view> {
    #[must_use]
    pub const fn new(view: &'view dyn WorkspaceView) -> Self {
        Self { view }
    }

    pub fn plan(
        &self,
        request: ExtractionRequest,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        let derived = self.derive_plan(&request, budget)?;
        ExtractionPlan::new_budgeted(
            self.view.workspace_id(),
            self.view.revision(),
            request,
            derived.selection_witness,
            derived.sources,
            derived.artifacts,
            budget,
        )
        .map_err(map_model_error)
    }

    /// Re-derives feature-neutral selection and filter evidence before execution.
    pub(in crate::extraction) fn verify<'plan>(
        &self,
        plan: &'plan ExtractionPlan,
        budget: &mut AssetLoadBudget,
    ) -> Result<VerifiedExecutionPlan<'plan>, ExtractionPlanError> {
        if plan.workspace_id() != self.view.workspace_id()
            || plan.revision() != self.view.revision()
        {
            return Err(ExtractionPlanError::PlanContextMismatch);
        }
        #[cfg(feature = "decode")]
        {
            let derived = self.derive_plan(plan.request(), budget)?;
            verify_derived_plan(plan, &derived)?;
        }
        #[cfg(not(feature = "decode"))]
        {
            let SelectionDerivation {
                sources,
                candidates,
                witness,
                ..
            } = self.derive_selection(plan.request(), budget)?;
            if witness != plan.selection_witness() {
                return Err(ExtractionPlanError::PlanDerivationMismatch {
                    kind: ExtractionPlanMismatchKind::SelectionWitness,
                });
            }
            self.verify_artifact_projection(plan, &sources, candidates, budget)?;
            verify_no_decode_representations(plan)?;
        }
        Ok(VerifiedExecutionPlan { plan })
    }

    #[cfg(not(feature = "decode"))]
    fn verify_artifact_projection(
        &self,
        plan: &ExtractionPlan,
        sources: &[WorkspaceSource],
        candidates: Vec<(ObjectAddress, RevisionedObjectHandle)>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), ExtractionPlanError> {
        let mut artifact_index = 0_usize;
        for (address, handle) in candidates {
            let object = self.view.read_object(&handle, budget)?;
            if !plan
                .request()
                .filter()
                .matches_class(object.class().class_id(), object.class().class_name())
            {
                continue;
            }
            let object_name = object_name(&object, budget)?;
            if !plan
                .request()
                .filter()
                .matches_object_name(object_name.as_deref())
            {
                continue;
            }
            if plan.request().filter().limit().is_some_and(|limit| {
                u64::try_from(artifact_index).is_ok_and(|count| count >= limit)
            }) {
                break;
            }
            let Some(artifact) = plan.artifacts().get(artifact_index) else {
                return Err(ExtractionPlanError::PlanDerivationMismatch {
                    kind: ExtractionPlanMismatchKind::Artifacts,
                });
            };
            if artifact.address() != &address
                || artifact.class_id() != object.class().class_id()
                || artifact.class_name() != object.class().class_name()
                || artifact.object_name() != object_name.as_deref()
            {
                return Err(ExtractionPlanError::PlanDerivationMismatch {
                    kind: ExtractionPlanMismatchKind::Artifacts,
                });
            }
            let owner = source_for_id(handle.object().source(), sources)?;
            let expected_preferred = allocate_path(
                plan.request().prefix(),
                &address,
                owner.locator(),
                object.class().class_id(),
                object.class().class_name(),
                object_name.as_deref(),
                artifact.representation().preferred_extension(),
                false,
                budget,
            )?;
            let expected_fallback = artifact
                .representation()
                .fallback_extension()
                .map(|extension| {
                    allocate_path(
                        plan.request().prefix(),
                        &address,
                        owner.locator(),
                        object.class().class_id(),
                        object.class().class_name(),
                        object_name.as_deref(),
                        extension,
                        true,
                        budget,
                    )
                })
                .transpose()?;
            if artifact.preferred_path() != &expected_preferred
                || artifact.fallback_path() != expected_fallback.as_ref()
            {
                return Err(ExtractionPlanError::PlanDerivationMismatch {
                    kind: ExtractionPlanMismatchKind::ArtifactPaths,
                });
            }
            artifact_index += 1;
        }
        if artifact_index != plan.artifacts().len() {
            return Err(ExtractionPlanError::PlanDerivationMismatch {
                kind: ExtractionPlanMismatchKind::Artifacts,
            });
        }
        Ok(())
    }

    fn derive_plan(
        &self,
        request: &ExtractionRequest,
        budget: &mut AssetLoadBudget,
    ) -> Result<DerivedPlan, ExtractionPlanError> {
        let SelectionDerivation {
            sources,
            references,
            candidates,
            witness: selection_witness,
        } = self.derive_selection(request, budget)?;

        let mut representation_planner =
            RepresentationPlanner::new(self.view, references, &sources, request.representation());

        let mut source_expectations =
            budgeted_vec(sources.len(), "extraction source expectations", budget)?;
        let mut artifacts = budgeted_vec(candidates.len(), "extraction planned artifacts", budget)?;
        for (address, handle) in candidates {
            let object = self.view.read_object(&handle, budget)?;
            if !request
                .filter()
                .matches_class(object.class().class_id(), object.class().class_name())
            {
                continue;
            }
            let object_name = object_name(&object, budget)?;
            if !request.filter().matches_object_name(object_name.as_deref()) {
                continue;
            }
            if request.filter().limit().is_some_and(|limit| {
                u64::try_from(artifacts.len()).is_ok_and(|count| count >= limit)
            }) {
                break;
            }

            let owner = source_for_id(handle.object().source(), &sources)?;
            let ordinal = u32::try_from(artifacts.len()).map_err(|_| {
                ExtractionPlanError::ArithmeticOverflow {
                    resource: "extraction artifact ordinal",
                }
            })?;
            let choice = representation_planner.select(&address, &object, owner, budget)?;
            let representation = choice.finalize(
                ordinal,
                &address,
                &mut source_expectations,
                owner,
                budget,
                |extension, fallback, budget| {
                    allocate_path(
                        request.prefix(),
                        &address,
                        owner.locator(),
                        object.class().class_id(),
                        object.class().class_name(),
                        object_name.as_deref(),
                        extension,
                        fallback,
                        budget,
                    )
                },
            )?;
            let class_name = clone_string(
                object.class().class_name(),
                "extraction artifact class name",
                budget,
            )?;
            artifacts.push(
                PlannedArtifact::new(
                    ordinal,
                    address,
                    object.class().class_id(),
                    class_name,
                    object_name,
                    representation,
                )
                .map_err(map_model_error)?,
            );
        }

        source_expectations.sort_unstable_by(|left, right| left.locator().cmp(right.locator()));
        Ok(DerivedPlan {
            selection_witness,
            sources: source_expectations,
            artifacts,
        })
    }

    fn derive_selection(
        &self,
        request: &ExtractionRequest,
        budget: &mut AssetLoadBudget,
    ) -> Result<SelectionDerivation, ExtractionPlanError> {
        let sources = self.view.sources(budget)?;
        let references = reference_graph_for_request(self.view, request, budget)?;
        let handles = selected_handles(self.view, request, &sources, references.as_ref(), budget)?;
        let mut candidates = budgeted_vec(handles.len(), "extraction candidate addresses", budget)?;
        for handle in handles {
            let address = self.view.object_address(&handle, budget)?;
            candidates.push((address, handle));
        }
        candidates.sort_by(|left, right| left.0.cmp(&right.0));
        candidates.dedup_by(|left, right| left.0 == right.0);
        let witness = selection_witness(&candidates)?;
        Ok(SelectionDerivation {
            sources,
            references,
            candidates,
            witness,
        })
    }

    pub fn plan_handles(
        &self,
        handles: &[RevisionedObjectHandle],
        representation: ExtractionRepresentationPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        let mut addresses = budgeted_vec(handles.len(), "extraction handle addresses", budget)?;
        for handle in handles {
            handle.validate_context(self.view.workspace_id(), self.view.revision())?;
            addresses.push(self.view.object_address(handle, budget)?);
        }
        let request =
            ExtractionRequest::addresses(addresses, representation).map_err(map_model_error)?;
        self.plan(request, budget)
    }

    pub fn plan_bundle_containers(
        &self,
        pattern: &str,
        representation: ExtractionRepresentationPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        let pattern = clone_string(pattern, "bundle container query pattern", budget)?;
        let request = ExtractionRequest::bundle_container(pattern, representation)
            .map_err(map_model_error)?;
        self.plan(request, budget)
    }

    /// Returns every exact `AssetBundle.m_Container` reference occurrence without deduplication.
    pub fn bundle_container_occurrences(
        &self,
        query: BundleContainerQuery,
        budget: &mut AssetLoadBudget,
    ) -> Result<BundleContainerResult, ExtractionPlanError> {
        let references =
            ReferenceGraph::build(self.view, ReferenceGraphBuildOptions::unbounded(), budget)?;
        if !references.is_complete() {
            return Err(ExtractionPlanError::IncompleteReferenceGraph);
        }
        query_bundle_container_occurrences(self.view, &references, query, budget)
    }
}

struct DerivedPlan {
    selection_witness: ExtractionSelectionWitness,
    sources: Vec<super::contract::ExtractionSourceExpectation>,
    artifacts: Vec<PlannedArtifact>,
}

#[cfg(feature = "decode")]
fn verify_derived_plan(
    plan: &ExtractionPlan,
    derived: &DerivedPlan,
) -> Result<(), ExtractionPlanError> {
    if derived.selection_witness != plan.selection_witness() {
        return Err(ExtractionPlanError::PlanDerivationMismatch {
            kind: ExtractionPlanMismatchKind::SelectionWitness,
        });
    }
    if derived.sources != plan.sources() {
        return Err(ExtractionPlanError::PlanDerivationMismatch {
            kind: ExtractionPlanMismatchKind::SourceExpectations,
        });
    }
    if derived.artifacts.len() != plan.artifacts().len() {
        return Err(ExtractionPlanError::PlanDerivationMismatch {
            kind: ExtractionPlanMismatchKind::Artifacts,
        });
    }
    for (actual, expected) in plan.artifacts().iter().zip(&derived.artifacts) {
        if actual.ordinal() != expected.ordinal()
            || actual.address() != expected.address()
            || actual.class_id() != expected.class_id()
            || actual.class_name() != expected.class_name()
            || actual.object_name() != expected.object_name()
        {
            return Err(ExtractionPlanError::PlanDerivationMismatch {
                kind: ExtractionPlanMismatchKind::Artifacts,
            });
        }
        if actual.preferred_path() != expected.preferred_path()
            || actual.fallback_path() != expected.fallback_path()
        {
            return Err(ExtractionPlanError::PlanDerivationMismatch {
                kind: ExtractionPlanMismatchKind::ArtifactPaths,
            });
        }
        if actual.representation() != expected.representation() {
            return Err(ExtractionPlanError::PlanDerivationMismatch {
                kind: ExtractionPlanMismatchKind::Representations,
            });
        }
    }
    Ok(())
}

#[cfg(not(feature = "decode"))]
fn verify_no_decode_representations(plan: &ExtractionPlan) -> Result<(), ExtractionPlanError> {
    for artifact in plan.artifacts() {
        let representation = artifact.representation();
        let content = representation.preferred_content();
        let no_fallback = representation.fallback().is_none();
        let no_diagnostics = representation.diagnostics().is_empty();
        let exact = match artifact.address().kind() {
            ObjectKind::Yaml => {
                matches!(content, PlannedContent::Yaml) && no_fallback && no_diagnostics
            }
            ObjectKind::Binary => match plan.request().representation() {
                ExtractionRepresentationPolicy::RawOnly => {
                    matches!(content, PlannedContent::RawBinary) && no_fallback && no_diagnostics
                }
                ExtractionRepresentationPolicy::PreferDecoded
                    if artifact.class_id() == class_ids::TEXT_ASSET =>
                {
                    matches!(content, PlannedContent::TextAsset)
                        && representation.fallback_kind() == Some(ExtractionArtifactKind::BinaryRaw)
                        && no_diagnostics
                }
                ExtractionRepresentationPolicy::RequireDecoded
                    if artifact.class_id() == class_ids::TEXT_ASSET =>
                {
                    matches!(content, PlannedContent::TextAsset) && no_fallback && no_diagnostics
                }
                ExtractionRepresentationPolicy::PreferDecoded
                    if is_media_class(artifact.class_id()) =>
                {
                    media_content_matches_class(content, artifact.class_id())
                        && representation.fallback_kind() == Some(ExtractionArtifactKind::BinaryRaw)
                        && no_diagnostics
                }
                ExtractionRepresentationPolicy::RequireDecoded
                    if is_media_class(artifact.class_id()) =>
                {
                    if media_content_matches_class(content, artifact.class_id())
                        && no_fallback
                        && no_diagnostics
                    {
                        return Err(ExtractionPlanError::ExecutionCapabilityUnavailable {
                            ordinal: artifact.ordinal(),
                            capability: "media decode",
                        });
                    }
                    false
                }
                ExtractionRepresentationPolicy::PreferDecoded => {
                    matches!(content, PlannedContent::RawBinary)
                        && no_fallback
                        && representation.diagnostics().len() == 1
                        && representation.diagnostics()[0].code()
                            == ExtractionDiagnosticCode::UnsupportedClass
                }
                ExtractionRepresentationPolicy::RequireDecoded => false,
            },
        };
        if !exact {
            return Err(ExtractionPlanError::PlanDerivationMismatch {
                kind: ExtractionPlanMismatchKind::Representations,
            });
        }
    }
    Ok(())
}

#[cfg(not(feature = "decode"))]
const fn is_media_class(class_id: i32) -> bool {
    matches!(
        class_id,
        class_ids::AUDIO_CLIP | class_ids::TEXTURE_2D | class_ids::SPRITE
    )
}

#[cfg(not(feature = "decode"))]
const fn media_content_matches_class(content: &PlannedContent, class_id: i32) -> bool {
    matches!(
        (content, class_id),
        (PlannedContent::Audio { .. }, class_ids::AUDIO_CLIP)
            | (PlannedContent::TexturePng { .. }, class_ids::TEXTURE_2D)
            | (PlannedContent::SpritePng { .. }, class_ids::SPRITE)
    )
}

struct SelectionDerivation {
    sources: Vec<WorkspaceSource>,
    references: Option<ReferenceGraph>,
    candidates: Vec<(ObjectAddress, RevisionedObjectHandle)>,
    witness: ExtractionSelectionWitness,
}

fn map_model_error(error: ExtractionModelError) -> ExtractionPlanError {
    match error {
        ExtractionModelError::Budget(source) => ExtractionPlanError::Budget(source),
        ExtractionModelError::InvalidPath(
            unity_asset_write::artifact::ArtifactNameError::Budget(source),
        ) => ExtractionPlanError::Budget(source),
        ExtractionModelError::InvalidPath(
            unity_asset_write::artifact::ArtifactNameError::Allocation {
                resource,
                requested,
                source,
            },
        ) => ExtractionPlanError::Allocation {
            resource,
            requested,
            unit: ExtractionAllocationUnit::Bytes,
            source,
        },
        ExtractionModelError::Allocation {
            resource,
            requested,
            source,
        } => ExtractionPlanError::Allocation {
            resource,
            requested,
            unit: ExtractionAllocationUnit::CapacityUnits,
            source,
        },
        other => ExtractionPlanError::ModelValidation(Box::new(other)),
    }
}

fn reference_graph_for_request(
    view: &dyn WorkspaceView,
    request: &ExtractionRequest,
    budget: &mut AssetLoadBudget,
) -> Result<Option<ReferenceGraph>, ExtractionPlanError> {
    let required = matches!(
        request.selection(),
        ExtractionSelection::BundleContainer { .. }
            | ExtractionSelection::ReferenceTraversal { .. }
    );
    if !required {
        return Ok(None);
    }
    let references = ReferenceGraph::build(view, ReferenceGraphBuildOptions::unbounded(), budget)?;
    if !references.is_complete() {
        return Err(ExtractionPlanError::IncompleteReferenceGraph);
    }
    Ok(Some(references))
}

fn selection_witness(
    candidates: &[(ObjectAddress, RevisionedObjectHandle)],
) -> Result<ExtractionSelectionWitness, ExtractionPlanError> {
    let candidate_count = usize_to_u64(candidates.len(), "extraction selection witness")?;
    let candidate_digest = canonical_digest(&SelectionWitnessPayload {
        contract: "unity_asset.extraction_selection_witness",
        version: 2,
        addresses: CandidateAddresses(candidates),
    })
    .map_err(ExtractionModelError::from)
    .map_err(map_model_error)?;
    Ok(ExtractionSelectionWitness::new(
        candidate_count,
        candidate_digest,
    ))
}

#[derive(Serialize)]
struct SelectionWitnessPayload<'candidate> {
    contract: &'static str,
    version: u8,
    addresses: CandidateAddresses<'candidate>,
}

struct CandidateAddresses<'candidate>(&'candidate [(ObjectAddress, RevisionedObjectHandle)]);

impl Serialize for CandidateAddresses<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for (address, _) in self.0 {
            sequence.serialize_element(address)?;
        }
        sequence.end()
    }
}

fn selected_handles(
    view: &dyn WorkspaceView,
    request: &ExtractionRequest,
    sources: &[WorkspaceSource],
    references: Option<&ReferenceGraph>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<RevisionedObjectHandle>, ExtractionPlanError> {
    match request.selection() {
        ExtractionSelection::All => view.objects(budget).map_err(Into::into),
        ExtractionSelection::Sources { sources: selected } => {
            let mut handles = view.objects(budget)?;
            handles.retain(|handle| {
                source_for_id(handle.object().source(), sources)
                    .is_ok_and(|source| selected.binary_search(source.locator()).is_ok())
            });
            Ok(handles)
        }
        ExtractionSelection::Addresses { addresses } => {
            let mut handles = budgeted_vec(addresses.len(), "extraction selected handles", budget)?;
            for address in addresses {
                handles.push(resolve_required_handle(view, address, budget)?);
            }
            Ok(handles)
        }
        ExtractionSelection::BundleContainer { pattern } => {
            let references = references.ok_or(ExtractionPlanError::ReferenceInvariant(
                "bundle-container selection graph was not constructed",
            ))?;
            let pattern = clone_string(pattern, "bundle container query pattern", budget)?;
            let result = query_bundle_container_occurrences(
                view,
                references,
                BundleContainerQuery::new(pattern)?,
                budget,
            )?;
            if !result.is_complete() {
                return Err(ExtractionPlanError::IncompleteReferenceGraph);
            }
            let addresses = resolved_addresses(&result, budget)?;
            let mut handles =
                budgeted_vec(addresses.len(), "bundle container selected handles", budget)?;
            for address in addresses {
                handles.push(resolve_required_handle(view, &address, budget)?);
            }
            Ok(handles)
        }
        ExtractionSelection::ReferenceTraversal {
            roots,
            direction,
            limits,
        } => {
            let references = references.ok_or(ExtractionPlanError::ReferenceInvariant(
                "reference-traversal selection graph was not constructed",
            ))?;
            let mut root_handles = budgeted_vec(roots.len(), "extraction traversal roots", budget)?;
            for root in roots {
                root_handles.push(resolve_required_handle(view, root, budget)?);
            }
            let traversal = references.closure(
                &root_handles,
                direction.as_reference(),
                limits.as_reference(),
                budget,
            )?;
            if !traversal.is_complete() {
                return Err(ExtractionPlanError::IncompleteReferenceTraversal);
            }
            let mut handles =
                budgeted_vec(traversal.len(), "extraction traversal handles", budget)?;
            handles.extend(traversal.nodes().cloned());
            Ok(handles)
        }
    }
}

fn object_name(
    object: &WorkspaceObject,
    budget: &mut AssetLoadBudget,
) -> Result<Option<String>, ExtractionPlanError> {
    object
        .class()
        .get("m_Name")
        .or_else(|| object.class().get("name"))
        .and_then(UnityValue::as_str)
        .map(|name| clone_string(name, "extraction artifact object name", budget))
        .transpose()
}

fn allocate_path(
    prefix: Option<&ExtractionPath>,
    address: &ObjectAddress,
    source: &SourceLocator,
    class_id: i32,
    class_name: &str,
    object_name: Option<&str>,
    extension: &str,
    raw_fallback: bool,
    budget: &mut AssetLoadBudget,
) -> Result<ExtractionPath, ExtractionPlanError> {
    const SOURCE_LIMIT: usize = 48;
    const CLASS_LIMIT: usize = 48;
    const NAME_LIMIT: usize = 64;
    const DIGEST_HEX_BYTES: usize = 64;

    let digest = canonical_digest(address)
        .map_err(|error| ExtractionPlanError::CanonicalAddress(error.to_string()))?;
    let source_name = source.root_alias().as_str();
    let artifact_name = if let Some(name) = object_name {
        ArtifactStem::Text(name)
    } else if let Some(file_id) = address.yaml_file_id() {
        ArtifactStem::YamlFileId(file_id)
    } else if address.kind() == ObjectKind::Binary {
        ArtifactStem::Text("object")
    } else {
        ArtifactStem::Text("document")
    };
    let fallback = if raw_fallback { ".raw" } else { "" };
    let requested = [
        prefix.map_or(0, |prefix| prefix.as_str().len() + 1),
        "sources/source-".len(),
        slug_capacity_bound(source_name, SOURCE_LIMIT),
        "/class-".len(),
        11,
        "-".len(),
        slug_capacity_bound(class_name, CLASS_LIMIT),
        "/".len(),
        artifact_name.capacity_bound(NAME_LIMIT),
        "--".len(),
        DIGEST_HEX_BYTES,
        fallback.len(),
        ".".len(),
        extension.len(),
    ]
    .into_iter()
    .try_fold(0_usize, |total, part| total.checked_add(part))
    .ok_or(ExtractionPlanError::ArithmeticOverflow {
        resource: "extraction relative path",
    })?;
    budget.check_bytes(usize_to_u64(requested, "extraction relative path")?)?;
    let mut relative = String::new();
    relative
        .try_reserve_exact(requested)
        .map_err(|source| ExtractionPlanError::Allocation {
            resource: "extraction relative path",
            requested,
            unit: ExtractionAllocationUnit::Bytes,
            source,
        })?;
    if let Some(prefix) = prefix {
        relative.push_str(prefix.as_str());
        relative.push('/');
    }
    relative.push_str("sources/source-");
    push_slug(&mut relative, source_name, SOURCE_LIMIT);
    relative.push_str("/class-");
    write!(&mut relative, "{class_id}").map_err(|_| ExtractionPlanError::PathFormatting)?;
    relative.push('-');
    push_slug(&mut relative, class_name, CLASS_LIMIT);
    relative.push('/');
    artifact_name.push_slug(&mut relative, NAME_LIMIT)?;
    relative.push_str("--");
    push_digest_hex(&mut relative, digest);
    relative.push_str(fallback);
    relative.push('.');
    relative.push_str(extension);

    ExtractionPath::from_string_with_budget(relative, budget)
        .map_err(ExtractionModelError::from)
        .map_err(map_model_error)
}

#[derive(Clone, Copy)]
enum ArtifactStem<'name> {
    Text(&'name str),
    YamlFileId(YamlFileId),
}

impl ArtifactStem<'_> {
    fn capacity_bound(self, maximum: usize) -> usize {
        match self {
            Self::Text(value) => slug_capacity_bound(value, maximum),
            Self::YamlFileId(file_id) => "file-id-".len() + decimal_i64_len(file_id.get()),
        }
    }

    fn push_slug(self, output: &mut String, maximum: usize) -> Result<(), ExtractionPlanError> {
        match self {
            Self::Text(value) => {
                push_slug(output, value, maximum);
                Ok(())
            }
            Self::YamlFileId(file_id) => {
                output.push_str("file-id-");
                write!(output, "{file_id}").map_err(|_| ExtractionPlanError::PathFormatting)
            }
        }
    }
}

fn decimal_i64_len(value: i64) -> usize {
    let digits = value.unsigned_abs().ilog10() as usize + 1;
    digits + usize::from(value.is_negative())
}

const fn slug_capacity_bound(value: &str, maximum: usize) -> usize {
    let input_bound = if value.len() < maximum {
        value.len()
    } else {
        maximum
    };
    if input_bound < "unnamed".len() {
        "unnamed".len()
    } else {
        input_bound
    }
}

fn push_slug(output: &mut String, value: &str, maximum: usize) {
    let start = output.len();
    let mut separator = false;
    for character in value.chars() {
        let mapped = if character.is_ascii_alphanumeric() {
            Some(character.to_ascii_lowercase())
        } else if matches!(character, '-' | '_') {
            Some(character)
        } else {
            None
        };
        match mapped {
            Some(character) if output.len() - start < maximum => {
                output.push(character);
                separator = false;
            }
            Some(_) => break,
            None if !separator && output.len() != start && output.len() - start < maximum => {
                output.push('_');
                separator = true;
            }
            None => {}
        }
    }
    while output.len() != start && (output.ends_with('_') || output.ends_with('-')) {
        output.pop();
    }
    if output.len() == start {
        output.push_str("unnamed");
    }
}

fn push_digest_hex(output: &mut String, digest: unity_asset_core::DigestV1) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &byte in digest.as_bytes() {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colliding_media_slugs_keep_stable_identity_paths() {
        let source = SourceLocator::path("media.assets").unwrap();
        let texture = ObjectAddress::binary_direct(source.clone(), 41).unwrap();
        let sprite = ObjectAddress::binary_direct(source.clone(), 42).unwrap();
        let mut budget = AssetLoadBudget::default();

        let texture_path = allocate_path(
            None,
            &texture,
            &source,
            class_ids::TEXTURE_2D,
            "Texture2D",
            Some("UI/Hero Icon"),
            "png",
            false,
            &mut budget,
        )
        .unwrap();
        let sprite_path = allocate_path(
            None,
            &sprite,
            &source,
            class_ids::SPRITE,
            "Sprite",
            Some("UI?Hero Icon"),
            "png",
            false,
            &mut budget,
        )
        .unwrap();

        assert_ne!(texture_path, sprite_path);
        assert!(texture_path.as_str().contains("/ui_hero_icon--"));
        assert!(sprite_path.as_str().contains("/ui_hero_icon--"));

        let repeated = allocate_path(
            None,
            &texture,
            &source,
            class_ids::TEXTURE_2D,
            "Texture2D",
            Some("UI/Hero Icon"),
            "png",
            false,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(texture_path, repeated);
    }

    #[test]
    fn allocated_path_charges_every_retained_name_byte() {
        let source = SourceLocator::path("media.assets").unwrap();
        let address = ObjectAddress::binary_direct(source.clone(), 41).unwrap();
        let mut budget = AssetLoadBudget::default();

        let path = allocate_path(
            None,
            &address,
            &source,
            class_ids::TEXTURE_2D,
            "Texture2D",
            Some("UI/Hero Icon"),
            "png",
            false,
            &mut budget,
        )
        .unwrap();

        assert_eq!(budget.usage().bytes, path.retained_bytes().unwrap());
    }

    #[cfg(feature = "decode")]
    #[test]
    fn decoded_batch_builds_the_streamed_resource_index_once() {
        let sample = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/samples/char_118_yuki.ab");
        let mut workspace = crate::workspace::AssetWorkspace::new().unwrap();
        workspace
            .load_path(&sample, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        StreamedResourceResolver::reset_test_build_count();

        let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
            .with_filter(
                super::super::model::ExtractionFilter::new(
                    [class_ids::AUDIO_CLIP],
                    None,
                    None,
                    None,
                )
                .unwrap(),
            );
        let plan = ExtractionPlanner::new(&snapshot)
            .plan(request, &mut AssetLoadBudget::default())
            .unwrap();

        assert!(plan.artifacts().len() > 1, "fixture must exercise a batch");
        assert_eq!(StreamedResourceResolver::test_build_count(), 1);
    }

    #[cfg(feature = "decode")]
    #[test]
    fn streamed_media_plan_persists_the_exact_resolved_source() {
        let sample = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/samples/char_118_yuki.ab");
        let mut workspace = crate::workspace::AssetWorkspace::new().unwrap();
        workspace
            .load_path(&sample, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
            .with_filter(
                super::super::model::ExtractionFilter::new(
                    [class_ids::AUDIO_CLIP],
                    None,
                    None,
                    None,
                )
                .unwrap(),
            );
        let plan = ExtractionPlanner::new(&snapshot)
            .plan(request, &mut AssetLoadBudget::default())
            .unwrap();
        let mut encoded = serde_json::to_value(&plan).unwrap();
        let streamed_index = encoded["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .position(|artifact| artifact["preferred_content"]["stream"].is_object())
            .expect("fixture must contain a streamed AudioClip");
        let streamed = encoded["artifacts"][streamed_index].clone();

        assert!(
            streamed["preferred_content"]["stream"]
                .get("source")
                .is_some(),
            "streamed content must persist the exact resolved sidecar"
        );

        encoded["artifacts"][streamed_index]["preferred_content"]["stream"]
            .as_object_mut()
            .unwrap()
            .remove("source");
        let error = serde_json::from_value::<ExtractionPlan>(encoded).unwrap_err();
        assert!(error.to_string().contains("missing field `source`"));
    }

    #[cfg(not(feature = "decode"))]
    #[test]
    fn unavailable_decode_does_not_build_a_streamed_resource_index() {
        let sample = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/samples/banner_1");
        let mut workspace = crate::workspace::AssetWorkspace::new().unwrap();
        workspace
            .load_path(&sample, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        StreamedResourceResolver::reset_test_build_count();

        let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded);
        ExtractionPlanner::new(&snapshot)
            .plan(request, &mut AssetLoadBudget::default())
            .expect_err("decode must remain unavailable without the feature");

        assert_eq!(StreamedResourceResolver::test_build_count(), 0);
    }
}
