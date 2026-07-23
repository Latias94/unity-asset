use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use indexmap::IndexMap;
use unity_asset_binary::asset::SerializedFile;
use unity_asset_core::{
    AssetLoadBudget, Diagnostic, DiagnosticSeverity, DigestV1, FieldPath, ObjectAddress, ObjectId,
    ObjectKind, RevisionedObjectHandle, SourceFingerprint, SourceId, SourceKind, SourceLocator,
    UnityDocument, UnityValue, arc_value_allocation_bytes, index_map_allocation_bytes,
    string_allocation_bytes, vec_allocation_bytes,
};
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactBuildError, ArtifactBuildFailurePhase,
    ArtifactHandle, ArtifactPayload, LogicalArtifactName, OutputSlot, PreparedArtifactSet,
};
use unity_asset_write::object::{
    PreparedUnsafeRawObject, SerializedFieldGuard, SerializedObjectCandidate,
    SerializedObjectEncoder, SerializedObjectGuard, SerializedObjectMutation,
    SerializedSequenceEdit, SerializedValueSchema, UnsafeRawObjectAcknowledgement,
};
use unity_asset_write::resources::StreamedResourceFlags;
use unity_asset_write::serialized_file::{
    BudgetedExternalPath, ExternalTableAllocator, SerializedFileEdits, SerializedFileSource,
    SerializedFileWriter,
};
use unity_asset_yaml::UnityYamlSerializer;

use super::artifact_graph::{PreparedArtifactGraph, prepare_artifact_graph};
use super::destination::{
    DestinationExpectation, DestinationProofError, DestinationProofSet, DestinationState,
    PublicationDestination,
};
use super::reference::StagedReferenceMutationCodec;
use super::resource::{
    DeclaredResourceSidecar, FinishedResourceSidecar, ResourcePayloadInput, ResourceSidecarBuilder,
    ResourceSidecarLocation,
};
use super::source_proof::{PhysicalDependencyProofError, PhysicalDependencyProofSet};
use super::yaml::{
    FinishedYamlObject, YamlObjectCandidate, YamlSemanticOperation, YamlSequenceEdit,
};
use super::{
    PREPARE_REPORT_VERSION, PrepareArtifactReport, PrepareDiagnostic, PrepareError,
    PrepareFailureReport, PrepareOptions, PrepareReport, PrepareStage, PreparedChange,
    PreparedSourceReport,
};
use crate::schema::protected_plain_field_owner;
use crate::workspace::overlay::{PreparedSourceBinding, PreparedState};
use crate::workspace::plan::{
    GenericMutation, MutationOperation, MutationPlan, MutationValueOwned, PlanPayload,
    SequenceMutation,
};
use crate::workspace::source_catalog::{
    CatalogError, LocatorResolution, PhysicalDomainChange, SourceCatalog, SourceLocationKind,
};
use crate::workspace::{AssetWorkspace, WorkspaceLookup, WorkspaceSnapshot, WorkspaceView};

const MAX_RUNNER_DIAGNOSTIC_BYTES: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrepareCheckpoint {
    SourceValidationComplete,
    DestinationObservationComplete,
}

pub(super) fn prepare(
    workspace: &AssetWorkspace,
    plan: MutationPlan,
    options: PrepareOptions,
    budget: &mut AssetLoadBudget,
) -> Result<PreparedChange, PrepareError> {
    let mut observer = |_: PrepareCheckpoint| {};
    prepare_with_observer(workspace, plan, options, budget, &mut observer)
}

#[cfg(test)]
pub(super) fn prepare_with_test_observer(
    workspace: &AssetWorkspace,
    plan: MutationPlan,
    options: PrepareOptions,
    budget: &mut AssetLoadBudget,
    observer: &mut impl FnMut(PrepareCheckpoint),
) -> Result<PreparedChange, PrepareError> {
    prepare_with_observer(workspace, plan, options, budget, observer)
}

fn prepare_with_observer(
    workspace: &AssetWorkspace,
    plan: MutationPlan,
    options: PrepareOptions,
    budget: &mut AssetLoadBudget,
    observer: &mut impl FnMut(PrepareCheckpoint),
) -> Result<PreparedChange, PrepareError> {
    let snapshot = workspace.snapshot();
    let plan_digest = plan.digest().map_err(|error| {
        reject(
            &snapshot,
            None,
            RunnerFailure::new(
                None,
                PrepareStage::PlanIdentity,
                "PREPARE_PLAN_IDENTITY_REJECTED",
                error.to_string(),
            ),
        )
    })?;
    let (base_revision, sources, payloads, operations) = plan.into_parts();
    let operation_count = u32::try_from(operations.len()).map_err(|_| {
        reject(
            &snapshot,
            Some(plan_digest),
            RunnerFailure::new(
                None,
                PrepareStage::PlanIdentity,
                "PREPARE_OPERATION_COUNT_OVERFLOW",
                "mutation operation count does not fit the report contract",
            ),
        )
    })?;

    if base_revision != snapshot.revision() {
        return Err(reject(
            &snapshot,
            Some(plan_digest),
            RunnerFailure::new(
                None,
                PrepareStage::PlanIdentity,
                "PREPARE_REVISION_MISMATCH",
                format!(
                    "plan revision {base_revision} does not match workspace revision {}",
                    snapshot.revision()
                ),
            ),
        ));
    }

    let source_proofs = validate_sources(&snapshot, &sources, budget)
        .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    observer(PrepareCheckpoint::SourceValidationComplete);

    let mut resource_manifest_index = None;
    let mut codec = None;
    let mut groups = SourceCandidateSets::new();
    let mut changed_objects = budgeted_vec(
        operations.len(),
        "prepare changed objects",
        PrepareStage::AddressResolution,
        budget,
    )
    .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    let mut operations = operations.into_vec().into_iter();
    while let Some(operation) = operations.next() {
        let ordinal = operation.ordinal();
        let action = operation.into_action();
        if mutation_requires_reference_codec(&action) && codec.is_none() {
            codec = Some(
                StagedReferenceMutationCodec::build(&snapshot, budget).map_err(|error| {
                    reject(
                        &snapshot,
                        Some(plan_digest),
                        RunnerFailure::new(
                            Some(ordinal),
                            PrepareStage::AddressResolution,
                            "PREPARE_REFERENCE_CODEC_REJECTED",
                            error.to_string(),
                        ),
                    )
                })?,
            );
        }
        stage_operation(
            &snapshot,
            codec.as_ref(),
            &mut groups,
            &mut changed_objects,
            &mut resource_manifest_index,
            operations.as_slice(),
            &payloads,
            ordinal,
            action,
            budget,
        )
        .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    }
    drop(codec.take());
    changed_objects.sort_unstable();
    changed_objects.dedup();

    let resource_domain_count = resource_manifest_index
        .as_ref()
        .map_or(0, |index| index.domains.len());
    let mut finished_resources = budgeted_vec(
        resource_domain_count,
        "prepare finished resource sidecars",
        PrepareStage::ResourceAllocation,
        budget,
    )
    .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    for domain in resource_manifest_index
        .take()
        .into_iter()
        .flat_map(|index| index.domains)
    {
        let builder = domain.builder.ok_or_else(|| {
            reject(
                &snapshot,
                Some(plan_digest),
                RunnerFailure::new(
                    None,
                    PrepareStage::ResourceAllocation,
                    "PREPARE_RESOURCE_DOMAIN_INCOMPLETE",
                    format!("resource domain {:?} was never initialized", domain.owner),
                ),
            )
        })?;
        let sidecar = builder.finish(budget).map_err(|error| {
            reject(
                &snapshot,
                Some(plan_digest),
                RunnerFailure::new(
                    None,
                    PrepareStage::ResourceAllocation,
                    "PREPARE_RESOURCE_FINISH_REJECTED",
                    error.to_string(),
                ),
            )
        })?;
        debug_assert_eq!(sidecar.catalog_update().location(), domain.location);
        finished_resources.push(FinishedResourceDomain {
            location: domain.location,
            sidecar,
        });
    }

    groups.sort_by_source();
    let publication_roots = predict_publication_roots(snapshot.state().catalog(), &groups, budget)
        .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    let pending_output_specs =
        build_output_specs(snapshot.state().catalog(), &publication_roots, budget)
            .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;

    let mut output_specs = budgeted_vec::<DeclaredOutputSpec>(
        pending_output_specs.len(),
        "prepare declared output specifications",
        PrepareStage::ArtifactDeclaration,
        budget,
    )
    .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    let group_count = groups.checked_len().ok_or_else(|| {
        reject(
            &snapshot,
            Some(plan_digest),
            RunnerFailure::new(
                None,
                PrepareStage::ArtifactDeclaration,
                "PREPARE_ALLOCATION_OVERFLOW",
                "changed source group count overflow",
            ),
        )
    })?;
    let mut leaves = budgeted_vec::<(SourceId, ArtifactHandle)>(
        group_count
            .checked_add(finished_resources.len())
            .ok_or_else(|| {
                reject(
                    &snapshot,
                    Some(plan_digest),
                    RunnerFailure::new(
                        None,
                        PrepareStage::ArtifactDeclaration,
                        "PREPARE_ALLOCATION_OVERFLOW",
                        "changed leaf count overflow",
                    ),
                )
            })?,
        "prepare changed leaves",
        PrepareStage::ArtifactDeclaration,
        budget,
    )
    .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    let resource_count = finished_resources.len();
    let mut declared_resources = budgeted_vec::<DeclaredResourceDomain<'_>>(
        resource_count,
        "prepare declared resource sidecars",
        PrepareStage::ArtifactDeclaration,
        budget,
    )
    .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    let mut self_bound_roots = budgeted_vec::<SelfBoundPublicationRoot>(
        resource_count,
        "prepare self-bound resource roots",
        PrepareStage::ArtifactEncoding,
        budget,
    )
    .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    let mut resource_transaction = if resource_count == 0 {
        None
    } else {
        Some(
            snapshot
                .state()
                .catalog()
                .begin_transaction(budget)
                .map_err(|error| {
                    reject(
                        &snapshot,
                        Some(plan_digest),
                        catalog_failure(PrepareStage::ResourceAllocation, error),
                    )
                })?,
        )
    };

    let mut artifact_budget = ArtifactBudget::new(options.artifact_limits()).map_err(|error| {
        reject(
            &snapshot,
            Some(plan_digest),
            RunnerFailure::new(
                None,
                PrepareStage::ArtifactDeclaration,
                "PREPARE_ARTIFACT_BUDGET_REJECTED",
                error.to_string(),
            ),
        )
    })?;
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, budget).map_err(|error| {
            reject(
                &snapshot,
                Some(plan_digest),
                artifact_failure(PrepareStage::ArtifactDeclaration, error),
            )
        })?;
    for spec in pending_output_specs {
        let slot = declaration.declare_output(spec.name).map_err(|error| {
            reject(
                &snapshot,
                Some(plan_digest),
                artifact_failure(PrepareStage::ArtifactDeclaration, error),
            )
        })?;
        output_specs.push(DeclaredOutputSpec {
            slot,
            destination: spec.destination,
        });
    }
    for resource in finished_resources {
        let sidecar = resource
            .sidecar
            .declare(&mut declaration)
            .map_err(|error| {
                reject(
                    &snapshot,
                    Some(plan_digest),
                    RunnerFailure::new(
                        None,
                        PrepareStage::ArtifactDeclaration,
                        "PREPARE_RESOURCE_DECLARATION_REJECTED",
                        error.to_string(),
                    ),
                )
            })?;
        let output_slot = sidecar.output_slot();
        declared_resources.push(DeclaredResourceDomain {
            location: resource.location,
            output_slot,
            sidecar,
        });
    }
    let mut batch = declaration.seal_output_names().map_err(|error| {
        reject(
            &snapshot,
            Some(plan_digest),
            artifact_failure(PrepareStage::ArtifactDeclaration, error),
        )
    })?;

    let SourceCandidateSets { binary, yaml } = groups;
    let mut binary_groups = binary.into_iter().peekable();
    let mut yaml_groups = yaml.into_iter().peekable();
    loop {
        let next_binary = binary_groups.peek().map(|group| group.source);
        let next_yaml = yaml_groups.peek().map(|group| group.source);
        let take_binary = match (next_binary, next_yaml) {
            (None, None) => break,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(binary), Some(yaml)) => binary <= yaml,
        };
        if take_binary {
            if let Some(group) = binary_groups.next() {
                let source = group.source;
                let artifact =
                    prepare_binary_leaf(&snapshot, group, &payloads, &mut batch, plan_digest)?;
                leaves.push((source, artifact));
            }
        } else if let Some(group) = yaml_groups.next() {
            let source = group.source;
            let artifact = prepare_yaml_leaf(&snapshot, group, &mut batch, plan_digest)?;
            leaves.push((source, artifact));
        }
    }

    for resource in declared_resources {
        let DeclaredResourceDomain {
            location,
            output_slot,
            sidecar,
        } = resource;
        let prepared = sidecar.prepare(&mut batch).map_err(|error| {
            let stage = artifact_build_stage(error.failure_phase());
            reject(
                &snapshot,
                Some(plan_digest),
                RunnerFailure::new(
                    None,
                    stage,
                    "PREPARE_RESOURCE_ARTIFACT_REJECTED",
                    error.to_string(),
                ),
            )
        })?;
        debug_assert_eq!(prepared.catalog_update().location(), location);
        let (artifact, update) = prepared.into_parts();
        let source = inspect_runner_budget(&mut batch, |load_budget| {
            update
                .apply(
                    resource_transaction
                        .as_mut()
                        .expect("resource sidecars require a catalog transaction"),
                    load_budget,
                )
                .map_err(|error| catalog_failure(PrepareStage::ResourceAllocation, error))
        })
        .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
        leaves.push((source, artifact));
        if location.publication_root() {
            let output_slot = output_slot.ok_or_else(|| {
                reject(
                    &snapshot,
                    Some(plan_digest),
                    RunnerFailure::new(
                        None,
                        PrepareStage::ArtifactDeclaration,
                        "PREPARE_RESOURCE_OUTPUT_MISSING",
                        "companion resource did not retain its declared output slot",
                    ),
                )
            })?;
            self_bound_roots.push(SelfBoundPublicationRoot {
                source,
                artifact,
                output_slot,
            });
        } else if output_slot.is_some() {
            return Err(reject(
                &snapshot,
                Some(plan_digest),
                RunnerFailure::new(
                    None,
                    PrepareStage::ArtifactDeclaration,
                    "PREPARE_CONTAINED_RESOURCE_OUTPUT",
                    "contained resource unexpectedly declared a public output",
                ),
            ));
        }
    }
    let extended_catalog = match resource_transaction {
        Some(transaction) => Some(
            inspect_runner_budget(&mut batch, |load_budget| {
                transaction
                    .commit(load_budget)
                    .map_err(|error| catalog_failure(PrepareStage::ResourceAllocation, error))
            })
            .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?,
        ),
        None => None,
    };
    let graph_catalog = extended_catalog
        .as_ref()
        .unwrap_or_else(|| snapshot.state().catalog());
    for root in &self_bound_roots {
        inspect_runner_budget(&mut batch, |load_budget| {
            let spec = build_declared_output_spec(
                graph_catalog,
                root.source,
                root.output_slot,
                load_budget,
            )?;
            let insertion = output_specs.len();
            budgeted_vec_insert(
                &mut output_specs,
                insertion,
                spec,
                "prepare declared resource output specifications",
                PrepareStage::DestinationValidation,
                load_budget,
            )
        })
        .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    }
    leaves.sort_unstable_by_key(|(source, _)| *source);
    self_bound_roots.sort_unstable_by_key(|root| root.source);

    let graph =
        prepare_artifact_graph(&snapshot, graph_catalog, &mut batch, &leaves).map_err(|error| {
            let stage = artifact_build_stage(error.failure_phase());
            reject(
                &snapshot,
                Some(plan_digest),
                RunnerFailure::new(
                    None,
                    stage,
                    "PREPARE_ARTIFACT_GRAPH_REJECTED",
                    error.to_string(),
                ),
            )
        })?;
    bind_publication_roots(
        &mut batch,
        &graph,
        &publication_roots,
        &output_specs,
        &self_bound_roots,
    )
    .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    let artifacts = batch.finish().map_err(|error| {
        reject(
            &snapshot,
            Some(plan_digest),
            artifact_failure(PrepareStage::ArtifactEncoding, error),
        )
    })?;
    let artifacts = retain_artifact_set(artifacts, budget)
        .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    let artifact_usage = artifact_budget.usage();

    let publication_bindings = build_publication_bindings(&artifacts, &output_specs, budget)
        .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;

    let mut destinations = budgeted_vec::<PublicationDestination<'_>>(
        publication_bindings.len(),
        "prepare publication destinations",
        PrepareStage::DestinationValidation,
        budget,
    )
    .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    for binding in &publication_bindings {
        destinations.push(PublicationDestination::exact(
            binding.output,
            &binding.destination.target,
            binding.destination.expectation,
        ));
    }
    source_proofs.revalidate(budget).map_err(|error| {
        reject(
            &snapshot,
            Some(plan_digest),
            source_proof_failure(snapshot.state().catalog(), error, budget),
        )
    })?;
    let destination_proofs = DestinationProofSet::observe(&artifacts, &destinations, budget)
        .map_err(|error| {
            reject(
                &snapshot,
                Some(plan_digest),
                destination_failure(
                    error,
                    "PREPARE_DESTINATION_REJECTED",
                    &publication_bindings,
                    graph_catalog,
                    budget,
                ),
            )
        })?;
    observer(PrepareCheckpoint::DestinationObservationComplete);

    let candidate_catalog = build_candidate_catalog(graph_catalog, &graph, &artifacts, budget)
        .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    let prepared_revision = candidate_catalog.revision().map_err(|error| {
        reject(
            &snapshot,
            Some(plan_digest),
            RunnerFailure::new(
                None,
                PrepareStage::PreparedView,
                "PREPARE_CANDIDATE_REVISION_REJECTED",
                error.to_string(),
            ),
        )
    })?;
    let (source_bindings, source_reports) = build_source_bindings_and_reports(
        snapshot.state().catalog(),
        graph_catalog,
        &graph,
        &leaves,
        &artifacts,
        budget,
    )
    .map_err(|failure| reject(&snapshot, Some(plan_digest), failure))?;
    let state = PreparedState::new(
        snapshot.clone(),
        candidate_catalog,
        plan_digest,
        Arc::clone(&artifacts),
        source_bindings,
        budget,
    )
    .map_err(|error| {
        let stage = error.prepare_stage();
        let code = match stage {
            PrepareStage::IndependentReparse => "PREPARE_INDEPENDENT_REPARSE_FAILED",
            _ => "PREPARE_PREPARED_VIEW_FAILED",
        };
        reject(
            &snapshot,
            Some(plan_digest),
            RunnerFailure::new(None, stage, code, error.to_string()),
        )
    })?;

    destination_proofs.revalidate(budget).map_err(|error| {
        reject(
            &snapshot,
            Some(plan_digest),
            destination_failure(
                error,
                "PREPARE_DESTINATION_CHANGED",
                &publication_bindings,
                graph_catalog,
                budget,
            ),
        )
    })?;
    source_proofs.revalidate(budget).map_err(|error| {
        reject(
            &snapshot,
            Some(plan_digest),
            source_proof_failure(snapshot.state().catalog(), error, budget),
        )
    })?;
    if workspace.revision() != base_revision {
        return Err(reject(
            &snapshot,
            Some(plan_digest),
            RunnerFailure::new(
                None,
                PrepareStage::SourceValidation,
                "PREPARE_WORKSPACE_CHANGED",
                "workspace revision changed while preparing the candidate",
            ),
        ));
    }

    let footprint = artifacts.footprint();
    let counters = artifacts.build_counters();
    let report = PrepareReport {
        version: PREPARE_REPORT_VERSION,
        workspace_id: snapshot.workspace_id(),
        base_revision,
        prepared_revision,
        plan_digest,
        operation_count,
        sources: source_reports,
        artifacts: PrepareArtifactReport {
            outputs: footprint.outputs(),
            proof_images: footprint.proof_images(),
            publication_bytes: footprint.publication_bytes(),
            proof_bytes: footprint.proof_bytes(),
            generated_bytes: footprint.generated_bytes(),
            metadata_bytes: footprint.metadata_bytes(),
            pinned_source_bytes: footprint.pinned_source_bytes(),
            retained_bytes: footprint.retained_bytes(),
            referenced_source_bytes: footprint.referenced_source_bytes(),
            segments: footprint.segments(),
            source_ranges: counters.source_ranges(),
            generated_chunks: counters.generated_chunks(),
            digest_passes: counters.digest_passes(),
            digest_reuses: counters.digest_reuses(),
            validation_passes: counters.validation_passes(),
            peak_scratch_bytes: artifact_usage.peak_scratch_bytes(),
        },
    };
    Ok(PreparedChange {
        state,
        report,
        artifact_usage,
        changed_objects,
        source_proofs,
        destination_proofs,
    })
}

#[derive(Debug)]
struct SourceCandidateSets<'snapshot> {
    binary: Vec<BinarySourceCandidate<'snapshot>>,
    yaml: Vec<YamlSourceCandidate>,
}

impl SourceCandidateSets<'_> {
    const fn new() -> Self {
        Self {
            binary: Vec::new(),
            yaml: Vec::new(),
        }
    }

    fn checked_len(&self) -> Option<usize> {
        self.binary.len().checked_add(self.yaml.len())
    }

    fn sort_by_source(&mut self) {
        self.binary.sort_unstable_by_key(|group| group.source);
        self.yaml.sort_unstable_by_key(|group| group.source);
    }

    fn sources(&self) -> impl Iterator<Item = SourceId> + '_ {
        let mut binary_index = 0;
        let mut yaml_index = 0;
        std::iter::from_fn(move || {
            let binary = self.binary.get(binary_index).map(|group| group.source);
            let yaml = self.yaml.get(yaml_index).map(|group| group.source);
            match (binary, yaml) {
                (None, None) => None,
                (Some(source), None) => {
                    binary_index += 1;
                    Some(source)
                }
                (None, Some(source)) => {
                    yaml_index += 1;
                    Some(source)
                }
                (Some(binary), Some(yaml)) if binary <= yaml => {
                    binary_index += 1;
                    Some(binary)
                }
                (Some(_), Some(yaml)) => {
                    yaml_index += 1;
                    Some(yaml)
                }
            }
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum SourceCandidateIndex {
    Binary(usize),
    Yaml(usize),
}

#[derive(Debug)]
struct BinarySourceCandidate<'snapshot> {
    source: SourceId,
    file: &'snapshot SerializedFile,
    externals: ExternalTableAllocator<'snapshot>,
    objects: Vec<BinaryCandidateEntry>,
    semantic: Vec<Option<SerializedObjectCandidate<'snapshot>>>,
    unsafe_raw: Vec<Option<BinaryUnsafeRawCandidate<'snapshot>>>,
}

#[derive(Debug)]
struct BinaryCandidateEntry {
    owner: RevisionedObjectHandle,
    state: BinaryCandidateState,
    last_ordinal: u32,
}

#[derive(Debug, Clone, Copy)]
enum BinaryCandidateState {
    Semantic(usize),
    UnsafeRaw(usize),
}

#[derive(Debug)]
struct BinaryUnsafeRawCandidate<'snapshot> {
    prepared: PreparedUnsafeRawObject<'snapshot>,
    payload: DigestV1,
}

#[derive(Debug)]
struct YamlSourceCandidate {
    source: SourceId,
    objects: Vec<YamlCandidateEntry>,
}

#[derive(Debug)]
struct YamlCandidateEntry {
    owner: RevisionedObjectHandle,
    candidate: YamlObjectCandidate,
    last_ordinal: u32,
}

#[derive(Debug)]
struct ResourceDomainSlot<'payload> {
    owner: SourceId,
    location: ResourceSidecarLocation,
    entries: Range<usize>,
    builder: Option<ResourceSidecarBuilder<'payload>>,
}

#[derive(Debug, Clone, Copy)]
struct ResourceRoutingEntry<'payload> {
    location: ResourceSidecarLocation,
    ordinal: u32,
    payload: ResourcePayloadInput<'payload>,
}

#[derive(Debug)]
struct ResourceManifestIndex<'payload> {
    entries: Vec<ResourceRoutingEntry<'payload>>,
    domains: Vec<ResourceDomainSlot<'payload>>,
    #[cfg(test)]
    classified_operations: usize,
}

struct ResourceDomainAccess<'state, 'catalog, 'payload> {
    catalog: &'catalog SourceCatalog,
    index: &'state mut Option<ResourceManifestIndex<'payload>>,
    remaining: &'state [MutationOperation],
    payloads: &'payload [PlanPayload],
}

struct FinishedResourceDomain<'payload> {
    location: ResourceSidecarLocation,
    sidecar: FinishedResourceSidecar<'payload>,
}

struct DeclaredResourceDomain<'payload> {
    location: ResourceSidecarLocation,
    output_slot: Option<OutputSlot>,
    sidecar: DeclaredResourceSidecar<'payload>,
}

#[derive(Debug, Clone, Copy)]
struct SelfBoundPublicationRoot {
    source: SourceId,
    artifact: ArtifactHandle,
    output_slot: OutputSlot,
}

fn resource_sidecar_location(
    catalog: &SourceCatalog,
    source: SourceId,
) -> Result<ResourceSidecarLocation, CatalogError> {
    if !matches!(source.kind(), SourceKind::Yaml | SourceKind::SerializedFile) {
        return Err(CatalogError::StreamedResourceRequiresSidecar);
    }
    let Some(parent) = catalog.parent(source)? else {
        return Ok(ResourceSidecarLocation::Companion { parent: source });
    };
    if matches!(
        parent.kind(),
        SourceKind::Archive | SourceKind::WebFile | SourceKind::AssetBundle
    ) {
        Ok(ResourceSidecarLocation::Contained { container: parent })
    } else {
        Err(CatalogError::StreamedResourceRequiresSidecar)
    }
}

impl<'payload> ResourceManifestIndex<'payload> {
    fn build(
        catalog: &SourceCatalog,
        current_ordinal: u32,
        current_source: SourceId,
        current_payload: DigestV1,
        remaining: &[MutationOperation],
        payloads: &'payload [PlanPayload],
        target: &ObjectAddress,
        field: Option<&FieldPath>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, RunnerFailure> {
        let location = resource_sidecar_location(catalog, current_source).map_err(|error| {
            RunnerFailure::operation_at(
                current_ordinal,
                PrepareStage::ResourceAllocation,
                "PREPARE_RESOURCE_DOMAIN_REJECTED",
                error.to_string(),
                target.clone(),
                field.cloned(),
            )
        })?;
        let capacity = remaining
            .iter()
            .filter(|operation| {
                matches!(operation.action(), GenericMutation::ResourceReplace { .. })
            })
            .count()
            .checked_add(1)
            .ok_or_else(|| {
                RunnerFailure::operation_at(
                    current_ordinal,
                    PrepareStage::ResourceAllocation,
                    "PREPARE_RESOURCE_INDEX_OVERFLOW",
                    "resource manifest index entry count overflowed usize",
                    target.clone(),
                    field.cloned(),
                )
            })?;
        let mut entries = budgeted_vec(
            capacity,
            "prepare resource manifest index",
            PrepareStage::ResourceAllocation,
            budget,
        )
        .map_err(|failure| {
            failure.with_operation_context(
                current_ordinal,
                PrepareStage::ResourceAllocation,
                "PREPARE_RESOURCE_INDEX_REJECTED",
                target.clone(),
                field.cloned(),
            )
        })?;
        let payload = resource_payload_input(current_ordinal, current_payload, payloads)
            .ok_or_else(|| {
                RunnerFailure::operation_at(
                    current_ordinal,
                    PrepareStage::ResourceAllocation,
                    "PREPARE_RESOURCE_PAYLOAD_MISSING",
                    format!("plan payload {current_payload} is missing"),
                    target.clone(),
                    field.cloned(),
                )
            })?;
        entries.push(ResourceRoutingEntry {
            location,
            ordinal: current_ordinal,
            payload,
        });

        #[cfg(test)]
        let mut classified_operations = 0_usize;

        for operation in remaining {
            let GenericMutation::ResourceReplace { payload, .. } = operation.action() else {
                continue;
            };
            let operation_location = resource_operation_location(catalog, operation.action());
            #[cfg(test)]
            {
                classified_operations += 1;
            }
            let Some(location) = operation_location else {
                // Source expectations are global preconditions. Operation-local resolution that
                // survives them remains owned by the operation's ordinal, so the index omits it.
                continue;
            };
            let Some(payload_input) =
                resource_payload_input(operation.ordinal(), *payload, payloads)
            else {
                continue;
            };
            entries.push(ResourceRoutingEntry {
                location,
                ordinal: operation.ordinal(),
                payload: payload_input,
            });
        }
        entries.sort_unstable_by(|left, right| {
            left.location
                .parent()
                .cmp(&right.location.parent())
                .then_with(|| left.ordinal.cmp(&right.ordinal))
        });
        let domain_count = entries
            .iter()
            .enumerate()
            .filter(|(index, entry)| {
                *index == 0 || entries[*index - 1].location.parent() != entry.location.parent()
            })
            .count();
        let mut domains = budgeted_vec(
            domain_count,
            "prepare resource domain slots",
            PrepareStage::ResourceAllocation,
            budget,
        )
        .map_err(|failure| {
            failure.with_operation_context(
                current_ordinal,
                PrepareStage::ResourceAllocation,
                "PREPARE_RESOURCE_INDEX_REJECTED",
                target.clone(),
                field.cloned(),
            )
        })?;
        let mut start = 0_usize;
        while start < entries.len() {
            let location = entries[start].location;
            let owner = location.parent();
            let end =
                start + entries[start..].partition_point(|entry| entry.location.parent() <= owner);
            debug_assert!(
                entries[start..end]
                    .iter()
                    .all(|entry| entry.location == location)
            );
            domains.push(ResourceDomainSlot {
                owner,
                location,
                entries: start..end,
                builder: None,
            });
            start = end;
        }
        debug_assert_eq!(domains.len(), domain_count);
        Ok(Self {
            entries,
            domains,
            #[cfg(test)]
            classified_operations,
        })
    }

    #[cfg(test)]
    fn domain_entries(
        &self,
        location: ResourceSidecarLocation,
    ) -> &[ResourceRoutingEntry<'payload>] {
        let owner = location.parent();
        let Ok(index) = self
            .domains
            .binary_search_by_key(&owner, |domain| domain.owner)
        else {
            return &[];
        };
        let domain = &self.domains[index];
        debug_assert_eq!(domain.location, location);
        &self.entries[domain.entries.clone()]
    }
}

impl<'payload> ResourceDomainAccess<'_, '_, 'payload> {
    fn get_or_prepare(
        &mut self,
        ordinal: u32,
        source: SourceId,
        payload: DigestV1,
        target: &ObjectAddress,
        field: Option<&FieldPath>,
        budget: &mut AssetLoadBudget,
    ) -> Result<&mut ResourceSidecarBuilder<'payload>, RunnerFailure> {
        let location = resource_sidecar_location(self.catalog, source).map_err(|error| {
            RunnerFailure::operation_at(
                ordinal,
                PrepareStage::ResourceAllocation,
                "PREPARE_RESOURCE_DOMAIN_REJECTED",
                error.to_string(),
                target.clone(),
                field.cloned(),
            )
        })?;
        if self.index.is_none() {
            *self.index = Some(ResourceManifestIndex::build(
                self.catalog,
                ordinal,
                source,
                payload,
                self.remaining,
                self.payloads,
                target,
                field,
                budget,
            )?);
        }
        let index = self
            .index
            .as_mut()
            .expect("resource manifest index is initialized above");
        let owner = location.parent();
        let domain_index = match index
            .domains
            .binary_search_by_key(&owner, |domain| domain.owner)
        {
            Ok(index) => index,
            Err(_) => {
                return Err(RunnerFailure::operation_at(
                    ordinal,
                    PrepareStage::ResourceAllocation,
                    "PREPARE_RESOURCE_INDEX_MISSING",
                    "resource operation has no fixed domain slot",
                    target.clone(),
                    field.cloned(),
                ));
            }
        };
        let entries_range = index.domains[domain_index].entries.clone();
        let entries = &index.entries[entries_range];
        if index.domains[domain_index].location != location
            || entries
                .binary_search_by_key(&ordinal, |entry| entry.ordinal)
                .is_err()
        {
            return Err(RunnerFailure::operation_at(
                ordinal,
                PrepareStage::ResourceAllocation,
                "PREPARE_RESOURCE_INDEX_MISSING",
                "resource operation is missing from its manifest index",
                target.clone(),
                field.cloned(),
            ));
        }
        if index.domains[domain_index].builder.is_none() {
            let builder =
                prepare_resource_domain(location, entries, budget).map_err(|failure| {
                    failure.with_operation_context(
                        ordinal,
                        PrepareStage::ResourceAllocation,
                        "PREPARE_RESOURCE_MANIFEST_REJECTED",
                        target.clone(),
                        field.cloned(),
                    )
                })?;
            index.domains[domain_index].builder = Some(builder);
        }
        Ok(index.domains[domain_index]
            .builder
            .as_mut()
            .expect("resource domain builder is initialized above"))
    }
}

fn prepare_resource_domain<'payload>(
    location: ResourceSidecarLocation,
    entries: &[ResourceRoutingEntry<'payload>],
    budget: &mut AssetLoadBudget,
) -> Result<ResourceSidecarBuilder<'payload>, RunnerFailure> {
    let owner = location.parent();
    let expected_count = entries.len();
    let base_name = budgeted_resource_base_name(owner, budget)?;
    let builder = ResourceSidecarBuilder::content_addressed(
        location,
        StreamedResourceFlags::default(),
        None,
        &base_name,
        expected_count,
        entries.iter().map(|entry| entry.payload),
        budget,
    )
    .map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ResourceAllocation,
            "PREPARE_RESOURCE_MANIFEST_REJECTED",
            error.to_string(),
        )
    })?;
    Ok(builder)
}

fn resource_operation_location(
    catalog: &SourceCatalog,
    action: &GenericMutation,
) -> Option<ResourceSidecarLocation> {
    let GenericMutation::ResourceReplace { target, .. } = action else {
        return None;
    };
    let LocatorResolution::Resolved(source) = catalog.classify_locator(target.source_locator())
    else {
        return None;
    };
    resource_sidecar_location(catalog, source).ok()
}

fn resource_payload_input<'payload>(
    ordinal: u32,
    payload: DigestV1,
    payloads: &'payload [PlanPayload],
) -> Option<ResourcePayloadInput<'payload>> {
    let payload_index = payloads
        .binary_search_by_key(&payload, PlanPayload::digest)
        .ok()?;
    let plan_payload = &payloads[payload_index];
    Some(ResourcePayloadInput::new(
        ordinal,
        payload,
        plan_payload.bytes().as_slice(),
    ))
}

fn budgeted_resource_base_name(
    owner: SourceId,
    budget: &mut AssetLoadBudget,
) -> Result<String, RunnerFailure> {
    const NAME_BYTES: usize = "CAB-".len() + 32 + ".resS".len();
    let minimum = u64::try_from(NAME_BYTES).map_err(|_| {
        RunnerFailure::new(
            None,
            PrepareStage::ResourceAllocation,
            "PREPARE_RESOURCE_NAME_OVERFLOW",
            "resource sidecar base name does not fit the load-budget ledger",
        )
    })?;
    budget.check_bytes(minimum).map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ResourceAllocation,
            "PREPARE_RESOURCE_NAME_REJECTED",
            error.to_string(),
        )
    })?;
    let mut name = String::new();
    name.try_reserve_exact(NAME_BYTES).map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ResourceAllocation,
            "PREPARE_RESOURCE_NAME_REJECTED",
            error.to_string(),
        )
    })?;
    write!(&mut name, "CAB-{:032x}.resS", owner.local()).map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ResourceAllocation,
            "PREPARE_RESOURCE_NAME_REJECTED",
            error.to_string(),
        )
    })?;
    let actual = string_allocation_bytes(name.capacity()).map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ResourceAllocation,
            "PREPARE_RESOURCE_NAME_OVERFLOW",
            error.to_string(),
        )
    })?;
    budget.check_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ResourceAllocation,
            "PREPARE_RESOURCE_NAME_REJECTED",
            error.to_string(),
        )
    })?;
    budget.consume_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ResourceAllocation,
            "PREPARE_RESOURCE_NAME_REJECTED",
            error.to_string(),
        )
    })?;
    Ok(name)
}

fn stage_operation<'snapshot, 'payload>(
    snapshot: &'snapshot WorkspaceSnapshot,
    codec: Option<&StagedReferenceMutationCodec<'_>>,
    groups: &mut SourceCandidateSets<'snapshot>,
    changed_objects: &mut Vec<ObjectId>,
    resource_manifest_index: &mut Option<ResourceManifestIndex<'payload>>,
    remaining_operations: &[MutationOperation],
    payloads: &'payload [PlanPayload],
    ordinal: u32,
    action: GenericMutation,
    budget: &mut AssetLoadBudget,
) -> Result<(), RunnerFailure> {
    let target = budgeted_object_address_clone(
        action.target(),
        "prepare mutation target",
        PrepareStage::AddressResolution,
        budget,
    )?;
    let handle = resolve_object(snapshot, ordinal, &target, budget)?;
    let retained = u64::try_from(handle.object().retained_clone_bytes()).map_err(|_| {
        RunnerFailure::operation_at(
            ordinal,
            PrepareStage::AddressResolution,
            "PREPARE_OBJECT_IDENTITY_OVERFLOW",
            "resolved object identity allocation does not fit the load-budget ledger",
            target.clone(),
            None,
        )
    })?;
    budget.check_bytes(retained).map_err(|error| {
        RunnerFailure::operation_at(
            ordinal,
            PrepareStage::AddressResolution,
            "PREPARE_OBJECT_IDENTITY_REJECTED",
            error.to_string(),
            target.clone(),
            None,
        )
    })?;
    let changed_object = handle.object().clone();
    budget.consume_bytes(retained).map_err(|error| {
        RunnerFailure::operation_at(
            ordinal,
            PrepareStage::AddressResolution,
            "PREPARE_OBJECT_IDENTITY_REJECTED",
            error.to_string(),
            target.clone(),
            None,
        )
    })?;
    changed_objects.push(changed_object);
    let source = handle.object().source();
    let group_index = match handle.object().kind() {
        ObjectKind::Yaml => {
            let index = match groups
                .yaml
                .binary_search_by_key(&source, |group| group.source)
            {
                Ok(index) => index,
                Err(index) => {
                    budgeted_vec_insert(
                        &mut groups.yaml,
                        index,
                        YamlSourceCandidate {
                            source,
                            objects: Vec::new(),
                        },
                        "prepare source candidates",
                        PrepareStage::Mutation,
                        budget,
                    )?;
                    index
                }
            };
            SourceCandidateIndex::Yaml(index)
        }
        ObjectKind::Binary => {
            let index = match groups
                .binary
                .binary_search_by_key(&source, |group| group.source)
            {
                Ok(index) => index,
                Err(index) => {
                    let file = snapshot
                        .state()
                        .store()
                        .get(source)
                        .and_then(|entry| entry.cached_serialized())
                        .map(Arc::as_ref)
                        .ok_or_else(|| {
                            RunnerFailure::operation(
                                ordinal,
                                "PREPARE_BINARY_BASE_MISSING",
                                format!("serialized source {source:?} has no frozen base file"),
                                target.clone(),
                                None,
                            )
                        })?;
                    let externals = ExternalTableAllocator::new(file).map_err(|error| {
                        RunnerFailure::operation(
                            ordinal,
                            "PREPARE_EXTERNAL_TABLE_REJECTED",
                            error.to_string(),
                            target.clone(),
                            None,
                        )
                    })?;
                    budgeted_vec_insert(
                        &mut groups.binary,
                        index,
                        BinarySourceCandidate {
                            source,
                            file,
                            externals,
                            objects: Vec::new(),
                            semantic: Vec::new(),
                            unsafe_raw: Vec::new(),
                        },
                        "prepare source candidates",
                        PrepareStage::Mutation,
                        budget,
                    )?;
                    index
                }
            };
            SourceCandidateIndex::Binary(index)
        }
    };
    let mut resources = ResourceDomainAccess {
        catalog: snapshot.state().catalog(),
        index: resource_manifest_index,
        remaining: remaining_operations,
        payloads,
    };
    match group_index {
        SourceCandidateIndex::Yaml(index) => stage_yaml_operation(
            snapshot,
            codec,
            &mut resources,
            &mut groups.yaml[index],
            handle,
            ordinal,
            action,
            target,
            budget,
        ),
        SourceCandidateIndex::Binary(index) => stage_binary_operation(
            codec,
            &mut resources,
            &mut groups.binary[index],
            handle,
            ordinal,
            action,
            target,
            budget,
        ),
    }
}

fn stage_yaml_operation(
    snapshot: &WorkspaceSnapshot,
    codec: Option<&StagedReferenceMutationCodec<'_>>,
    resources: &mut ResourceDomainAccess<'_, '_, '_>,
    group: &mut YamlSourceCandidate,
    handle: RevisionedObjectHandle,
    ordinal: u32,
    action: GenericMutation,
    target: ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<(), RunnerFailure> {
    let object_index = match group
        .objects
        .binary_search_by(|entry| entry.candidate.object().cmp(handle.object()))
    {
        Ok(index) => index,
        Err(index) => {
            let base = snapshot.read_object(&handle, budget).map_err(|error| {
                RunnerFailure::operation(
                    ordinal,
                    "PREPARE_OBJECT_READ_REJECTED",
                    error.to_string(),
                    target.clone(),
                    None,
                )
            })?;
            let candidate =
                YamlObjectCandidate::from_workspace_object(base, budget).map_err(|error| {
                    RunnerFailure::operation(
                        ordinal,
                        "PREPARE_CANDIDATE_REJECTED",
                        error.to_string(),
                        target.clone(),
                        None,
                    )
                })?;
            budgeted_vec_insert(
                &mut group.objects,
                index,
                YamlCandidateEntry {
                    owner: handle,
                    candidate,
                    last_ordinal: ordinal,
                },
                "prepare YAML object candidates",
                PrepareStage::Mutation,
                budget,
            )?;
            index
        }
    };
    let source = group.source;
    let entry = &mut group.objects[object_index];
    let field = mutation_field_path(&action)
        .map(|path| {
            budgeted_field_path_clone(
                path,
                "prepare mutation diagnostic field",
                PrepareStage::Mutation,
                budget,
            )
        })
        .transpose()?;
    let result = match action {
        GenericMutation::FieldReplace {
            path,
            guard,
            replacement,
            ..
        } => {
            if let Some(owner) = protected_plain_field_owner(
                entry.candidate.class().class_id,
                ObjectKind::Yaml,
                entry.candidate.class().properties(),
                &path,
            ) {
                return Err(RunnerFailure::operation(
                    ordinal,
                    "PREPARE_PROTECTED_SEMANTIC_FIELD",
                    format!("field is owned by the {owner} semantic recipe"),
                    target,
                    Some(path),
                ));
            }
            let replacement = {
                let current = entry
                    .candidate
                    .class()
                    .value_at_path(&path)
                    .map_err(|error| {
                        RunnerFailure::operation(
                            ordinal,
                            "PREPARE_MUTATION_REJECTED",
                            error.to_string(),
                            target.clone(),
                            Some(path.clone()),
                        )
                    })?;
                lower_yaml_mutation_value(codec, &entry.owner, Some(current), replacement, budget)
                    .map_err(|error| {
                    RunnerFailure::operation(
                        ordinal,
                        "PREPARE_MUTATION_REJECTED",
                        error.to_string(),
                        target.clone(),
                        Some(path.clone()),
                    )
                })?
            };
            entry.candidate.apply(
                YamlSemanticOperation::FieldReplace {
                    ordinal,
                    path: &path,
                    guard,
                    replacement,
                },
                budget,
            )
        }
        GenericMutation::ReferenceReplace {
            path,
            schema_digest,
            expected,
            replacement,
            ..
        } => {
            required_codec(codec, ordinal, &target, Some(&path))?
                .apply_yaml_reference_replace(
                    &entry.owner,
                    &mut entry.candidate,
                    ordinal,
                    &path,
                    schema_digest,
                    &expected,
                    &replacement,
                    budget,
                )
                .map_err(|error| {
                    RunnerFailure::operation(
                        ordinal,
                        "PREPARE_MUTATION_REJECTED",
                        error.to_string(),
                        target.clone(),
                        Some(path),
                    )
                })?;
            entry.last_ordinal = ordinal;
            return Ok(());
        }
        GenericMutation::SchemaReplace {
            guard, replacement, ..
        } => {
            let replacement =
                lower_yaml_mutation_value(codec, &entry.owner, None, replacement, budget).map_err(
                    |error| {
                        RunnerFailure::operation(
                            ordinal,
                            "PREPARE_MUTATION_REJECTED",
                            error.to_string(),
                            target.clone(),
                            None,
                        )
                    },
                )?;
            entry.candidate.apply(
                YamlSemanticOperation::SchemaReplace {
                    ordinal,
                    guard,
                    replacement,
                },
                budget,
            )
        }
        GenericMutation::SequenceEdit {
            path, guard, edit, ..
        } => {
            let edit = lower_yaml_sequence_edit(
                codec,
                &entry.owner,
                &entry.candidate,
                &path,
                edit,
                budget,
            )
            .map_err(|error| {
                RunnerFailure::operation(
                    ordinal,
                    "PREPARE_MUTATION_REJECTED",
                    error,
                    target.clone(),
                    Some(path.clone()),
                )
            })?;
            entry.candidate.apply(
                YamlSemanticOperation::SequenceEdit {
                    ordinal,
                    path: &path,
                    guard,
                    edit,
                },
                budget,
            )
        }
        GenericMutation::UnsafeRawReplace { .. } => entry
            .candidate
            .apply(YamlSemanticOperation::UnsafeRaw { ordinal }, budget),
        GenericMutation::ResourceReplace {
            path,
            guard,
            payload,
            ..
        } => {
            let validated = entry
                .candidate
                .validate_replace_field_guard(ordinal, path, guard, budget)
                .map_err(|error| {
                    RunnerFailure::operation_at(
                        ordinal,
                        PrepareStage::ResourceAllocation,
                        "PREPARE_RESOURCE_GUARD_REJECTED",
                        error.to_string(),
                        target.clone(),
                        field.clone(),
                    )
                })?;
            let resource = resources.get_or_prepare(
                ordinal,
                source,
                payload,
                &target,
                field.as_ref(),
                budget,
            )?;
            let preview = resource.preview_next(ordinal, payload).map_err(|error| {
                RunnerFailure::operation_at(
                    ordinal,
                    PrepareStage::ResourceAllocation,
                    "PREPARE_RESOURCE_PREVIEW_REJECTED",
                    error.to_string(),
                    target.clone(),
                    field.clone(),
                )
            })?;
            let replacement = {
                let current = entry
                    .candidate
                    .class()
                    .value_at_path(validated.path())
                    .map_err(|error| {
                        RunnerFailure::operation_at(
                            ordinal,
                            PrepareStage::ResourceAllocation,
                            "PREPARE_RESOURCE_FIELD_REJECTED",
                            error.to_string(),
                            target.clone(),
                            field.clone(),
                        )
                    })?;
                resource
                    .stage_preview(&preview, validated.path(), current, budget)
                    .map_err(|error| {
                        RunnerFailure::operation_at(
                            ordinal,
                            PrepareStage::ResourceAllocation,
                            "PREPARE_RESOURCE_FIELD_REJECTED",
                            error.to_string(),
                            target.clone(),
                            field.clone(),
                        )
                    })?
            };
            let prepared = entry
                .candidate
                .prepare_validated_replace_field(validated, replacement)
                .map_err(|error| {
                    RunnerFailure::operation_at(
                        ordinal,
                        PrepareStage::ResourceAllocation,
                        "PREPARE_RESOURCE_GUARD_REJECTED",
                        error.to_string(),
                        target.clone(),
                        field.clone(),
                    )
                })?;
            resource
                .commit_prepared(preview, prepared, budget)
                .map_err(|error| {
                    RunnerFailure::operation_at(
                        ordinal,
                        PrepareStage::ResourceAllocation,
                        "PREPARE_RESOURCE_COMMIT_REJECTED",
                        error.to_string(),
                        target,
                        field,
                    )
                })?;
            entry.last_ordinal = ordinal;
            return Ok(());
        }
    };
    result.map_err(|error| {
        RunnerFailure::operation(
            ordinal,
            "PREPARE_MUTATION_REJECTED",
            error.to_string(),
            target,
            field,
        )
    })?;
    entry.last_ordinal = ordinal;
    Ok(())
}

fn stage_binary_operation(
    codec: Option<&StagedReferenceMutationCodec<'_>>,
    resources: &mut ResourceDomainAccess<'_, '_, '_>,
    group: &mut BinarySourceCandidate<'_>,
    handle: RevisionedObjectHandle,
    ordinal: u32,
    action: GenericMutation,
    target: ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<(), RunnerFailure> {
    let path_id = handle.object().binary_path_id().ok_or_else(|| {
        RunnerFailure::operation(
            ordinal,
            "PREPARE_BINARY_IDENTITY_REJECTED",
            "binary object address has no path ID",
            target.clone(),
            None,
        )
    })?;
    let object_index = match group.objects.binary_search_by(|entry| {
        entry
            .owner
            .object()
            .binary_path_id()
            .unwrap_or_default()
            .cmp(&path_id)
    }) {
        Ok(index) => index,
        Err(index) => {
            let encoder = SerializedObjectEncoder::new(group.file, path_id).map_err(|error| {
                RunnerFailure::operation(
                    ordinal,
                    "PREPARE_CANDIDATE_REJECTED",
                    error.to_string(),
                    target.clone(),
                    None,
                )
            })?;
            let state = match &action {
                GenericMutation::UnsafeRawReplace {
                    expected_raw_digest,
                    payload,
                    ..
                } => {
                    let prepared =
                        encoder
                            .prepare_unsafe_raw(*expected_raw_digest)
                            .map_err(|error| {
                                RunnerFailure::operation(
                                    ordinal,
                                    "PREPARE_MUTATION_REJECTED",
                                    error.to_string(),
                                    target.clone(),
                                    None,
                                )
                            })?;
                    let raw_index = group.unsafe_raw.len();
                    budgeted_vec_insert(
                        &mut group.unsafe_raw,
                        raw_index,
                        Some(BinaryUnsafeRawCandidate {
                            prepared,
                            payload: *payload,
                        }),
                        "prepare binary object candidates",
                        PrepareStage::Mutation,
                        budget,
                    )?;
                    BinaryCandidateState::UnsafeRaw(raw_index)
                }
                _ => {
                    let candidate = encoder.begin_semantic(budget).map_err(|error| {
                        RunnerFailure::operation(
                            ordinal,
                            "PREPARE_CANDIDATE_REJECTED",
                            error.to_string(),
                            target.clone(),
                            None,
                        )
                    })?;
                    let semantic_index = group.semantic.len();
                    budgeted_vec_insert(
                        &mut group.semantic,
                        semantic_index,
                        Some(candidate),
                        "prepare binary object candidates",
                        PrepareStage::Mutation,
                        budget,
                    )?;
                    BinaryCandidateState::Semantic(semantic_index)
                }
            };
            budgeted_vec_insert(
                &mut group.objects,
                index,
                BinaryCandidateEntry {
                    owner: handle,
                    state,
                    last_ordinal: ordinal,
                },
                "prepare binary object candidates",
                PrepareStage::Mutation,
                budget,
            )?;
            index
        }
    };
    let field = mutation_field_path(&action)
        .map(|path| {
            budgeted_field_path_clone(
                path,
                "prepare mutation diagnostic field",
                PrepareStage::Mutation,
                budget,
            )
        })
        .transpose()?;
    let state = group.objects[object_index].state;
    let semantic_index = match state {
        BinaryCandidateState::Semantic(index) => index,
        BinaryCandidateState::UnsafeRaw(_) => {
            if matches!(action, GenericMutation::UnsafeRawReplace { .. }) {
                group.objects[object_index].last_ordinal = ordinal;
                return Ok(());
            }
            return Err(RunnerFailure::operation(
                ordinal,
                "PREPARE_MUTATION_REJECTED",
                "semantic mutation cannot follow an unsafe raw replacement",
                target,
                field,
            ));
        }
    };
    let source = group.source;
    let owner = &group.objects[object_index].owner;
    let Some(candidate) = group
        .semantic
        .get_mut(semantic_index)
        .and_then(|candidate| candidate.as_mut())
    else {
        return Err(RunnerFailure::operation(
            ordinal,
            "PREPARE_MUTATION_REJECTED",
            "binary semantic candidate arena entry is missing",
            target,
            field,
        ));
    };

    match action {
        GenericMutation::FieldReplace {
            path,
            guard,
            replacement,
            ..
        } => {
            let root = candidate
                .semantic_value()
                .as_object()
                .expect("a semantic SerializedFile candidate always has an object root");
            if let Some(owner) =
                protected_plain_field_owner(candidate.class_id(), ObjectKind::Binary, root, &path)
            {
                return Err(RunnerFailure::operation(
                    ordinal,
                    "PREPARE_PROTECTED_SEMANTIC_FIELD",
                    format!("field is owned by the {owner} semantic recipe"),
                    target,
                    Some(path),
                ));
            }
            let replacement = {
                let schema = candidate.value_schema_at_path(&path).map_err(|error| {
                    RunnerFailure::operation(
                        ordinal,
                        "PREPARE_MUTATION_REJECTED",
                        error.to_string(),
                        target.clone(),
                        Some(path.clone()),
                    )
                })?;
                let current = candidate.value_at_path(&path).map_err(|error| {
                    RunnerFailure::operation(
                        ordinal,
                        "PREPARE_MUTATION_REJECTED",
                        error.to_string(),
                        target.clone(),
                        Some(path.clone()),
                    )
                })?;
                lower_binary_mutation_value(
                    codec,
                    owner,
                    schema,
                    Some(current),
                    replacement,
                    &mut group.externals,
                    budget,
                )
                .map_err(|error| {
                    RunnerFailure::operation(
                        ordinal,
                        "PREPARE_MUTATION_REJECTED",
                        error.to_string(),
                        target.clone(),
                        Some(path.clone()),
                    )
                })?
            };
            candidate
                .apply(
                    SerializedObjectMutation::replace_field(
                        ordinal,
                        path,
                        SerializedFieldGuard::new(guard.schema_digest(), guard.value_digest()),
                        replacement,
                    ),
                    budget,
                )
                .map_err(|error| {
                    RunnerFailure::operation(
                        ordinal,
                        "PREPARE_MUTATION_REJECTED",
                        error.to_string(),
                        target.clone(),
                        field.clone(),
                    )
                })?;
        }
        GenericMutation::ReferenceReplace {
            path,
            schema_digest,
            expected,
            replacement,
            ..
        } => required_codec(codec, ordinal, &target, Some(&path))?
            .apply_binary_reference_replace(
                owner,
                candidate,
                &mut group.externals,
                ordinal,
                path,
                schema_digest,
                &expected,
                &replacement,
                budget,
            )
            .map_err(|error| {
                RunnerFailure::operation(
                    ordinal,
                    "PREPARE_MUTATION_REJECTED",
                    error.to_string(),
                    target.clone(),
                    field.clone(),
                )
            })?,
        GenericMutation::SchemaReplace {
            guard, replacement, ..
        } => {
            let replacement = {
                let schema = candidate.root_value_schema();
                let current = candidate
                    .value_at_path(&FieldPath::root())
                    .map_err(|error| {
                        RunnerFailure::operation(
                            ordinal,
                            "PREPARE_MUTATION_REJECTED",
                            error.to_string(),
                            target.clone(),
                            None,
                        )
                    })?;
                lower_binary_mutation_value(
                    codec,
                    owner,
                    schema,
                    Some(current),
                    replacement,
                    &mut group.externals,
                    budget,
                )
                .map_err(|error| {
                    RunnerFailure::operation(
                        ordinal,
                        "PREPARE_MUTATION_REJECTED",
                        error.to_string(),
                        target.clone(),
                        None,
                    )
                })?
            };
            let actual = replacement.kind();
            let UnityValue::Object(replacement) = replacement else {
                return Err(RunnerFailure::operation(
                    ordinal,
                    "PREPARE_MUTATION_REJECTED",
                    format!("binary schema replacement must be an object, found {actual:?}"),
                    target,
                    None,
                ));
            };
            candidate
                .apply(
                    SerializedObjectMutation::replace_object(
                        ordinal,
                        SerializedObjectGuard::new(guard.schema_digest(), guard.value_digest()),
                        replacement,
                    ),
                    budget,
                )
                .map_err(|error| {
                    RunnerFailure::operation(
                        ordinal,
                        "PREPARE_MUTATION_REJECTED",
                        error.to_string(),
                        target.clone(),
                        None,
                    )
                })?;
        }
        GenericMutation::SequenceEdit {
            path, guard, edit, ..
        } => {
            let edit = lower_binary_sequence_edit(
                codec,
                owner,
                candidate,
                &path,
                edit,
                &mut group.externals,
                budget,
            )
            .map_err(|message| {
                RunnerFailure::operation(
                    ordinal,
                    "PREPARE_MUTATION_REJECTED",
                    message,
                    target.clone(),
                    Some(path.clone()),
                )
            })?;
            candidate
                .apply(
                    SerializedObjectMutation::edit_sequence(
                        ordinal,
                        path,
                        SerializedFieldGuard::new(guard.schema_digest(), guard.value_digest()),
                        edit,
                    ),
                    budget,
                )
                .map_err(|error| {
                    RunnerFailure::operation(
                        ordinal,
                        "PREPARE_MUTATION_REJECTED",
                        error.to_string(),
                        target.clone(),
                        field.clone(),
                    )
                })?;
        }
        GenericMutation::ResourceReplace {
            path,
            guard,
            payload,
            ..
        } => {
            let validated = candidate
                .validate_replace_field_guard(
                    ordinal,
                    path,
                    SerializedFieldGuard::new(guard.schema_digest(), guard.value_digest()),
                    budget,
                )
                .map_err(|error| {
                    RunnerFailure::operation_at(
                        ordinal,
                        PrepareStage::ResourceAllocation,
                        "PREPARE_RESOURCE_GUARD_REJECTED",
                        error.to_string(),
                        target.clone(),
                        field.clone(),
                    )
                })?;
            let resource = resources.get_or_prepare(
                ordinal,
                source,
                payload,
                &target,
                field.as_ref(),
                budget,
            )?;
            let preview = resource.preview_next(ordinal, payload).map_err(|error| {
                RunnerFailure::operation_at(
                    ordinal,
                    PrepareStage::ResourceAllocation,
                    "PREPARE_RESOURCE_PREVIEW_REJECTED",
                    error.to_string(),
                    target.clone(),
                    field.clone(),
                )
            })?;
            let wire_path = match budgeted_binary_resource_wire_path(
                resource.location(),
                target.source_locator(),
                resource.member_name(),
                budget,
            ) {
                Ok(wire_path) => wire_path,
                Err(message) => {
                    return Err(RunnerFailure::operation_at(
                        ordinal,
                        PrepareStage::ResourceAllocation,
                        "PREPARE_RESOURCE_WIRE_PATH_REJECTED",
                        message,
                        target,
                        field,
                    ));
                }
            };
            let replacement = {
                let current = candidate.value_at_path(validated.path()).map_err(|error| {
                    RunnerFailure::operation_at(
                        ordinal,
                        PrepareStage::ResourceAllocation,
                        "PREPARE_RESOURCE_FIELD_REJECTED",
                        error.to_string(),
                        target.clone(),
                        field.clone(),
                    )
                })?;
                resource
                    .stage_preview_with_wire_path(
                        &preview,
                        validated.path(),
                        current,
                        wire_path.as_str(),
                        budget,
                    )
                    .map_err(|error| {
                        RunnerFailure::operation_at(
                            ordinal,
                            PrepareStage::ResourceAllocation,
                            "PREPARE_RESOURCE_FIELD_REJECTED",
                            error.to_string(),
                            target.clone(),
                            field.clone(),
                        )
                    })?
            };
            let prepared = candidate
                .prepare_validated_replace_field(validated, replacement, budget)
                .map_err(|error| {
                    RunnerFailure::operation_at(
                        ordinal,
                        PrepareStage::ResourceAllocation,
                        "PREPARE_RESOURCE_GUARD_REJECTED",
                        error.to_string(),
                        target.clone(),
                        field.clone(),
                    )
                })?;
            let prepared_external = group
                .externals
                .prepare_budgeted_path(wire_path, budget)
                .map_err(|error| {
                    RunnerFailure::operation_at(
                        ordinal,
                        PrepareStage::ResourceAllocation,
                        "PREPARE_RESOURCE_EXTERNAL_REJECTED",
                        error.to_string(),
                        target.clone(),
                        field.clone(),
                    )
                })?;
            resource
                .commit_prepared(preview, prepared, budget)
                .map_err(|error| {
                    RunnerFailure::operation_at(
                        ordinal,
                        PrepareStage::ResourceAllocation,
                        "PREPARE_RESOURCE_COMMIT_REJECTED",
                        error.to_string(),
                        target,
                        field,
                    )
                })?;
            prepared_external.commit();
        }
        GenericMutation::UnsafeRawReplace { .. } => {
            return Err(RunnerFailure::operation(
                ordinal,
                "PREPARE_MUTATION_REJECTED",
                "unsafe raw replacement cannot follow semantic mutations",
                target,
                None,
            ));
        }
    }
    group.objects[object_index].last_ordinal = ordinal;
    Ok(())
}

fn budgeted_binary_resource_wire_path(
    location: ResourceSidecarLocation,
    source: &SourceLocator,
    member_name: &str,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedExternalPath, String> {
    const ARCHIVE_PREFIX: &str = "archive:/";
    let serialized_member = match location {
        ResourceSidecarLocation::Companion { .. } => None,
        ResourceSidecarLocation::Contained { .. } => {
            let member = source
                .members()
                .last()
                .ok_or_else(|| "contained SerializedFile source has no member identity".to_owned())?
                .member()
                .name();
            let base_name = member
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| "contained SerializedFile member has no basename".to_owned())?;
            Some(base_name)
        }
    };
    let required = if let Some(serialized_member) = serialized_member {
        ARCHIVE_PREFIX
            .len()
            .checked_add(serialized_member.len())
            .and_then(|length| length.checked_add(1))
            .and_then(|length| length.checked_add(member_name.len()))
    } else {
        Some(member_name.len())
    }
    .ok_or_else(|| "binary resource wire path length overflowed usize".to_owned())?;
    let minimum = u64::try_from(required)
        .map_err(|_| "binary resource wire path length does not fit the load budget".to_owned())?;
    budget
        .check_bytes(minimum)
        .map_err(|error| error.to_string())?;

    let mut wire_path = String::new();
    wire_path
        .try_reserve_exact(required)
        .map_err(|error| error.to_string())?;
    let actual =
        string_allocation_bytes(wire_path.capacity()).map_err(|error| error.to_string())?;
    budget
        .check_bytes(actual)
        .map_err(|error| error.to_string())?;
    if let Some(serialized_member) = serialized_member {
        wire_path.push_str(ARCHIVE_PREFIX);
        wire_path.push_str(serialized_member);
        wire_path.push('/');
    }
    wire_path.push_str(member_name);
    debug_assert_eq!(wire_path.len(), required);
    BudgetedExternalPath::new(wire_path, budget).map_err(|error| error.to_string())
}

fn lower_binary_sequence_edit(
    codec: Option<&StagedReferenceMutationCodec<'_>>,
    owner: &RevisionedObjectHandle,
    candidate: &SerializedObjectCandidate<'_>,
    path: &FieldPath,
    edit: SequenceMutation,
    externals: &mut ExternalTableAllocator<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<SerializedSequenceEdit, String> {
    match edit {
        SequenceMutation::Insert { index, value } => {
            let schema = candidate
                .value_schema_at_path(path)
                .map_err(|error| error.to_string())?
                .element(0)
                .ok_or_else(|| "binary sequence has no element schema".to_owned())?;
            Ok(SerializedSequenceEdit::Insert {
                index,
                value: lower_binary_mutation_value(
                    codec, owner, schema, None, value, externals, budget,
                )?,
            })
        }
        SequenceMutation::Replace { index, value } => {
            let schema = candidate
                .value_schema_at_path(path)
                .map_err(|error| error.to_string())?
                .element(usize::try_from(index).unwrap_or(0))
                .ok_or_else(|| "binary sequence has no element schema".to_owned())?;
            let current = candidate
                .value_at_path(path)
                .map_err(|error| error.to_string())?
                .as_array()
                .and_then(|values| {
                    usize::try_from(index)
                        .ok()
                        .and_then(|index| values.get(index))
                });
            Ok(SerializedSequenceEdit::Replace {
                index,
                value: lower_binary_mutation_value(
                    codec, owner, schema, current, value, externals, budget,
                )?,
            })
        }
        SequenceMutation::Remove { index } => Ok(SerializedSequenceEdit::Remove { index }),
        SequenceMutation::Move { from, to } => Ok(SerializedSequenceEdit::Move { from, to }),
        SequenceMutation::Clear => Ok(SerializedSequenceEdit::Clear),
    }
}

fn lower_yaml_sequence_edit(
    codec: Option<&StagedReferenceMutationCodec<'_>>,
    owner: &RevisionedObjectHandle,
    candidate: &YamlObjectCandidate,
    path: &FieldPath,
    edit: SequenceMutation,
    budget: &mut AssetLoadBudget,
) -> Result<YamlSequenceEdit, String> {
    match edit {
        SequenceMutation::Insert { index, value } => Ok(YamlSequenceEdit::Insert {
            index,
            value: lower_yaml_mutation_value(codec, owner, None, value, budget)?,
        }),
        SequenceMutation::Replace { index, value } => {
            let current = candidate
                .class()
                .value_at_path(path)
                .map_err(|error| error.to_string())?
                .as_array()
                .and_then(|values| {
                    usize::try_from(index)
                        .ok()
                        .and_then(|index| values.get(index))
                });
            Ok(YamlSequenceEdit::Replace {
                index,
                value: lower_yaml_mutation_value(codec, owner, current, value, budget)?,
            })
        }
        SequenceMutation::Remove { index } => Ok(YamlSequenceEdit::Remove { index }),
        SequenceMutation::Move { from, to } => Ok(YamlSequenceEdit::Move { from, to }),
        SequenceMutation::Clear => Ok(YamlSequenceEdit::Clear),
    }
}

fn mutation_requires_reference_codec(action: &GenericMutation) -> bool {
    match action {
        GenericMutation::ReferenceReplace { .. } => true,
        GenericMutation::FieldReplace { replacement, .. }
        | GenericMutation::SchemaReplace { replacement, .. } => {
            mutation_value_contains_reference(replacement)
        }
        GenericMutation::SequenceEdit { edit, .. } => match edit {
            SequenceMutation::Insert { value, .. } | SequenceMutation::Replace { value, .. } => {
                mutation_value_contains_reference(value)
            }
            SequenceMutation::Remove { .. }
            | SequenceMutation::Move { .. }
            | SequenceMutation::Clear => false,
        },
        GenericMutation::ResourceReplace { .. } | GenericMutation::UnsafeRawReplace { .. } => false,
    }
}

fn mutation_value_contains_reference(value: &crate::workspace::MutationValue) -> bool {
    match value.view() {
        crate::workspace::MutationValueRef::Reference(_) => true,
        crate::workspace::MutationValueRef::Array(values) => {
            values.iter().any(mutation_value_contains_reference)
        }
        crate::workspace::MutationValueRef::Object(fields) => fields
            .iter()
            .any(|field| mutation_value_contains_reference(field.value())),
        crate::workspace::MutationValueRef::Null
        | crate::workspace::MutationValueRef::Bool(_)
        | crate::workspace::MutationValueRef::Signed(_)
        | crate::workspace::MutationValueRef::Unsigned(_)
        | crate::workspace::MutationValueRef::Float64(_)
        | crate::workspace::MutationValueRef::String(_)
        | crate::workspace::MutationValueRef::Bytes(_) => false,
    }
}

fn required_codec<'codec>(
    codec: Option<&'codec StagedReferenceMutationCodec<'_>>,
    ordinal: u32,
    target: &ObjectAddress,
    field: Option<&FieldPath>,
) -> Result<&'codec StagedReferenceMutationCodec<'codec>, RunnerFailure> {
    codec.ok_or_else(|| {
        RunnerFailure::operation(
            ordinal,
            "PREPARE_REFERENCE_CODEC_MISSING",
            "logical reference operation has no staged reference codec",
            target.clone(),
            field.cloned(),
        )
    })
}

fn lower_yaml_mutation_value(
    codec: Option<&StagedReferenceMutationCodec<'_>>,
    owner: &RevisionedObjectHandle,
    current: Option<&UnityValue>,
    value: crate::workspace::MutationValue,
    budget: &mut AssetLoadBudget,
) -> Result<UnityValue, String> {
    if mutation_value_contains_reference(&value) {
        return codec
            .ok_or_else(|| "logical reference value has no staged reference codec".to_owned())?
            .lower_yaml_mutation_value(owner, current, value, budget)
            .map_err(|error| error.to_string());
    }
    lower_plain_mutation_value(value.into_owned(), budget, 0)
}

fn lower_binary_mutation_value(
    codec: Option<&StagedReferenceMutationCodec<'_>>,
    owner: &RevisionedObjectHandle,
    schema: SerializedValueSchema<'_>,
    current: Option<&UnityValue>,
    value: crate::workspace::MutationValue,
    externals: &mut ExternalTableAllocator<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<UnityValue, String> {
    if mutation_value_contains_reference(&value) {
        return codec
            .ok_or_else(|| "logical reference value has no staged reference codec".to_owned())?
            .lower_binary_mutation_value(owner, schema, current, value, externals, budget)
            .map_err(|error| error.to_string());
    }
    lower_plain_mutation_value(value.into_owned(), budget, 0)
}

fn lower_plain_mutation_value(
    value: MutationValueOwned,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<UnityValue, String> {
    budget
        .observe_depth(depth)
        .map_err(|error| error.to_string())?;
    match value {
        MutationValueOwned::Null => Ok(UnityValue::Null),
        MutationValueOwned::Bool(value) => Ok(UnityValue::Bool(value)),
        MutationValueOwned::Signed(value) => Ok(UnityValue::Integer(value)),
        MutationValueOwned::Unsigned(value) => Ok(UnityValue::Unsigned(value)),
        MutationValueOwned::Float64(value) => Ok(UnityValue::Float(value.to_f64())),
        MutationValueOwned::String(value) => {
            let actual =
                string_allocation_bytes(value.capacity()).map_err(|error| error.to_string())?;
            budget
                .check_bytes(actual)
                .map_err(|error| error.to_string())?;
            budget
                .consume_bytes(actual)
                .map_err(|error| error.to_string())?;
            Ok(UnityValue::String(value))
        }
        MutationValueOwned::Bytes(value) => {
            let value = value.into_vec();
            let actual =
                vec_allocation_bytes::<u8>(value.capacity()).map_err(|error| error.to_string())?;
            budget
                .check_bytes(actual)
                .map_err(|error| error.to_string())?;
            budget
                .consume_bytes(actual)
                .map_err(|error| error.to_string())?;
            Ok(UnityValue::Bytes(value))
        }
        MutationValueOwned::Reference(_) => {
            Err("plain mutation lowering encountered a logical reference".to_owned())
        }
        MutationValueOwned::Array(values) => {
            let child_depth = depth
                .checked_add(1)
                .ok_or_else(|| "mutation value depth overflow".to_owned())?;
            let count = u64::try_from(values.len()).map_err(|error| error.to_string())?;
            let planned = vec_allocation_bytes::<UnityValue>(values.len())
                .map_err(|error| error.to_string())?;
            budget
                .check_members(count)
                .map_err(|error| error.to_string())?;
            budget
                .check_bytes(planned)
                .map_err(|error| error.to_string())?;
            let mut output = Vec::new();
            output
                .try_reserve_exact(values.len())
                .map_err(|error| error.to_string())?;
            let actual = vec_allocation_bytes::<UnityValue>(output.capacity())
                .map_err(|error| error.to_string())?;
            budget
                .check_bytes(actual)
                .map_err(|error| error.to_string())?;
            budget
                .consume_members(count)
                .map_err(|error| error.to_string())?;
            budget
                .consume_bytes(actual)
                .map_err(|error| error.to_string())?;
            for value in values {
                output.push(lower_plain_mutation_value(
                    value.into_owned(),
                    budget,
                    child_depth,
                )?);
            }
            Ok(UnityValue::Array(output))
        }
        MutationValueOwned::Object(fields) => {
            let child_depth = depth
                .checked_add(1)
                .ok_or_else(|| "mutation value depth overflow".to_owned())?;
            let count = u64::try_from(fields.len()).map_err(|error| error.to_string())?;
            let planned = index_map_allocation_bytes::<String, UnityValue>(fields.len())
                .map_err(|error| error.to_string())?;
            budget
                .check_members(count)
                .map_err(|error| error.to_string())?;
            budget
                .check_bytes(planned)
                .map_err(|error| error.to_string())?;
            let mut output = IndexMap::new();
            output
                .try_reserve_exact(fields.len())
                .map_err(|error| error.to_string())?;
            let actual = index_map_allocation_bytes::<String, UnityValue>(output.capacity())
                .map_err(|error| error.to_string())?;
            budget
                .check_bytes(actual)
                .map_err(|error| error.to_string())?;
            budget
                .consume_members(count)
                .map_err(|error| error.to_string())?;
            budget
                .consume_bytes(actual)
                .map_err(|error| error.to_string())?;
            for field in fields {
                let (name, value) = field.into_parts();
                let actual =
                    string_allocation_bytes(name.capacity()).map_err(|error| error.to_string())?;
                budget
                    .check_bytes(actual)
                    .map_err(|error| error.to_string())?;
                budget
                    .consume_bytes(actual)
                    .map_err(|error| error.to_string())?;
                output.insert(
                    name,
                    lower_plain_mutation_value(value.into_owned(), budget, child_depth)?,
                );
            }
            Ok(UnityValue::Object(output))
        }
    }
}

fn resolve_object(
    snapshot: &WorkspaceSnapshot,
    ordinal: u32,
    target: &ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<RevisionedObjectHandle, RunnerFailure> {
    match snapshot.resolve_object(target, budget).map_err(|error| {
        RunnerFailure::address(
            ordinal,
            "PREPARE_ADDRESS_RESOLUTION_FAILED",
            error.to_string(),
            target.clone(),
        )
    })? {
        WorkspaceLookup::Resolved(handle) => Ok(handle),
        WorkspaceLookup::Unloaded => Err(RunnerFailure::address(
            ordinal,
            "PREPARE_ADDRESS_UNLOADED",
            "object source is not loaded",
            target.clone(),
        )),
        WorkspaceLookup::Missing => Err(RunnerFailure::address(
            ordinal,
            "PREPARE_ADDRESS_MISSING",
            "object address does not exist",
            target.clone(),
        )),
        WorkspaceLookup::Ambiguous { candidates } => Err(RunnerFailure::address(
            ordinal,
            "PREPARE_ADDRESS_AMBIGUOUS",
            format!("object address resolved to {} candidates", candidates.len()),
            target.clone(),
        )),
        WorkspaceLookup::Invalid { diagnostic } => Err(RunnerFailure::address(
            ordinal,
            "PREPARE_ADDRESS_INVALID",
            diagnostic.message().to_owned(),
            target.clone(),
        )),
    }
}

fn finish_yaml_group(
    group: YamlSourceCandidate,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<FinishedYamlObject>, RunnerFailure> {
    let mut finished = budgeted_vec(
        group.objects.len(),
        "prepare finished YAML objects",
        PrepareStage::ArtifactEncoding,
        budget,
    )?;
    for entry in group.objects {
        finished.push(entry.candidate.finish());
    }
    finished.sort_unstable_by_key(FinishedYamlObject::document_index);
    Ok(finished)
}

fn prepare_yaml_leaf(
    snapshot: &WorkspaceSnapshot,
    group: YamlSourceCandidate,
    batch: &mut unity_asset_write::artifact::ArtifactBatch<'_, '_>,
    plan_digest: DigestV1,
) -> Result<ArtifactHandle, PrepareError> {
    let source = group.source;
    let finished = batch
        .inspect_with_budget(|load_budget| {
            Ok::<_, ArtifactBuildError>(finish_yaml_group(group, load_budget))
        })
        .map_err(|error| {
            reject(
                snapshot,
                Some(plan_digest),
                artifact_failure(PrepareStage::ArtifactEncoding, error),
            )
        })?
        .map_err(|failure| reject(snapshot, Some(plan_digest), failure))?;
    let document = snapshot
        .state()
        .store()
        .get(source)
        .and_then(|entry| entry.cached_yaml())
        .ok_or_else(|| {
            reject(
                snapshot,
                Some(plan_digest),
                RunnerFailure::new(
                    None,
                    PrepareStage::ArtifactEncoding,
                    "PREPARE_YAML_BASE_MISSING",
                    format!("YAML source {source:?} has no frozen base document"),
                ),
            )
        })?;
    let mut writer = batch.yaml_writer().map_err(|error| {
        reject(
            snapshot,
            Some(plan_digest),
            artifact_failure(PrepareStage::ArtifactEncoding, error),
        )
    })?;
    batch
        .inspect_with_budget(|load_budget| {
            let classes = document.entries().iter().enumerate().map(|(index, base)| {
                finished
                    .binary_search_by_key(&index, FinishedYamlObject::document_index)
                    .ok()
                    .map_or(base, |candidate| finished[candidate].class())
            });
            let mut serializer = UnityYamlSerializer::new();
            Ok::<_, ArtifactBuildError>(
                serializer
                    .serialize_to_writer_with_budget(&mut writer, classes, load_budget)
                    .map_err(|error| error.to_string()),
            )
        })
        .map_err(|error| {
            reject(
                snapshot,
                Some(plan_digest),
                artifact_failure(PrepareStage::ArtifactEncoding, error),
            )
        })?
        .map_err(|message| {
            reject(
                snapshot,
                Some(plan_digest),
                RunnerFailure::new(
                    None,
                    PrepareStage::ArtifactEncoding,
                    "PREPARE_YAML_ENCODING_REJECTED",
                    message,
                ),
            )
        })?;
    batch
        .prepare_yaml_writer(writer)
        .map_err(|error| reject(snapshot, Some(plan_digest), artifact_prepare_failure(error)))
}

fn prepare_binary_leaf(
    snapshot: &WorkspaceSnapshot,
    group: BinarySourceCandidate<'_>,
    payloads: &[PlanPayload],
    batch: &mut unity_asset_write::artifact::ArtifactBatch<'_, '_>,
    plan_digest: DigestV1,
) -> Result<ArtifactHandle, PrepareError> {
    let source = group.source;
    let file = group.file;
    let edits = batch
        .inspect_with_budget(|load_budget| {
            Ok::<_, ArtifactBuildError>(finish_binary_group(group, payloads, load_budget))
        })
        .map_err(|error| {
            reject(
                snapshot,
                Some(plan_digest),
                artifact_failure(PrepareStage::ArtifactEncoding, error),
            )
        })?
        .map_err(|failure| reject(snapshot, Some(plan_digest), failure))?;
    let image = snapshot
        .state()
        .store()
        .get(source)
        .map(|entry| entry.image())
        .ok_or_else(|| {
            reject(
                snapshot,
                Some(plan_digest),
                RunnerFailure::new(
                    None,
                    PrepareStage::ArtifactEncoding,
                    "PREPARE_BINARY_BASE_MISSING",
                    format!("serialized source {source:?} has no immutable base image"),
                ),
            )
        })?;
    let payload = ArtifactPayload::source_backed(source, image.clone()).map_err(|error| {
        reject(
            snapshot,
            Some(plan_digest),
            RunnerFailure::new(
                None,
                PrepareStage::ArtifactEncoding,
                "PREPARE_SOURCE_PAYLOAD_REJECTED",
                error.to_string(),
            ),
        )
    })?;
    let source = SerializedFileSource::whole(&payload).map_err(|error| {
        reject(
            snapshot,
            Some(plan_digest),
            RunnerFailure::new(
                None,
                PrepareStage::ArtifactEncoding,
                "PREPARE_SERIALIZED_SOURCE_REJECTED",
                error.to_string(),
            ),
        )
    })?;
    SerializedFileWriter::prepare(batch, file, &edits, Some(source))
        .map_err(|error| reject(snapshot, Some(plan_digest), artifact_prepare_failure(error)))
}

fn finish_binary_group(
    group: BinarySourceCandidate<'_>,
    payloads: &[PlanPayload],
    budget: &mut AssetLoadBudget,
) -> Result<SerializedFileEdits, RunnerFailure> {
    let BinarySourceCandidate {
        externals,
        objects,
        mut semantic,
        mut unsafe_raw,
        ..
    } = group;
    let mut edits = externals.finish();
    for entry in objects {
        let ordinal = entry.last_ordinal;
        let encoded = match entry.state {
            BinaryCandidateState::Semantic(index) => {
                let Some(candidate) = semantic
                    .get_mut(index)
                    .and_then(|candidate| candidate.take())
                else {
                    return Err(RunnerFailure::new(
                        Some(ordinal),
                        PrepareStage::ArtifactEncoding,
                        "PREPARE_BINARY_FINISH_REJECTED",
                        "binary semantic candidate arena entry is missing",
                    ));
                };
                candidate.finish(budget).map_err(|error| {
                    RunnerFailure::new(
                        Some(ordinal),
                        PrepareStage::ArtifactEncoding,
                        "PREPARE_BINARY_FINISH_REJECTED",
                        error.to_string(),
                    )
                })?
            }
            BinaryCandidateState::UnsafeRaw(index) => {
                let Some(raw) = unsafe_raw
                    .get_mut(index)
                    .and_then(|candidate| candidate.take())
                else {
                    return Err(RunnerFailure::new(
                        Some(ordinal),
                        PrepareStage::ArtifactEncoding,
                        "PREPARE_BINARY_FINISH_REJECTED",
                        "binary unsafe raw candidate arena entry is missing",
                    ));
                };
                let BinaryUnsafeRawCandidate { prepared, payload } = raw;
                let bytes = clone_plan_payload(payloads, payload, budget).map_err(|message| {
                    RunnerFailure::new(
                        Some(ordinal),
                        PrepareStage::ArtifactEncoding,
                        "PREPARE_PAYLOAD_REJECTED",
                        message,
                    )
                })?;
                prepared
                    .finish(
                        bytes,
                        UnsafeRawObjectAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
                        budget,
                    )
                    .map_err(|error| {
                        RunnerFailure::new(
                            Some(ordinal),
                            PrepareStage::ArtifactEncoding,
                            "PREPARE_BINARY_FINISH_REJECTED",
                            error.to_string(),
                        )
                    })?
            }
        };
        edits
            .try_insert_encoded_object(encoded, budget)
            .map_err(|error| {
                RunnerFailure::new(
                    Some(ordinal),
                    PrepareStage::ArtifactEncoding,
                    "PREPARE_BINARY_EDIT_REJECTED",
                    error.to_string(),
                )
            })?;
    }
    Ok(edits)
}

fn clone_plan_payload(
    payloads: &[PlanPayload],
    digest: DigestV1,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, String> {
    let payload = payloads
        .binary_search_by_key(&digest, PlanPayload::digest)
        .ok()
        .map(|index| &payloads[index])
        .ok_or_else(|| format!("plan payload {digest} is missing"))?;
    let bytes = payload.bytes().as_slice();
    let required = u64::try_from(bytes.len())
        .map_err(|_| "plan payload length does not fit the load budget".to_owned())?;
    budget
        .check_bytes(required)
        .map_err(|error| error.to_string())?;
    let mut copy = Vec::new();
    copy.try_reserve_exact(bytes.len())
        .map_err(|error| error.to_string())?;
    copy.extend_from_slice(bytes);
    let actual = vec_allocation_bytes::<u8>(copy.capacity()).map_err(|error| error.to_string())?;
    budget
        .check_bytes(actual)
        .map_err(|error| error.to_string())?;
    budget
        .consume_bytes(actual)
        .map_err(|error| error.to_string())?;
    Ok(copy)
}

fn validate_sources(
    snapshot: &WorkspaceSnapshot,
    expectations: &[crate::workspace::SourceExpectation],
    budget: &mut AssetLoadBudget,
) -> Result<PhysicalDependencyProofSet, RunnerFailure> {
    let catalog = snapshot.state().catalog();
    for expectation in expectations {
        let source = match catalog.classify_locator(expectation.locator()) {
            LocatorResolution::Resolved(source) => source,
            LocatorResolution::Unloaded => {
                return Err(RunnerFailure::source(
                    "PREPARE_SOURCE_UNLOADED",
                    "declared source is not loaded",
                    budgeted_source_locator_clone(
                        expectation.locator(),
                        "prepare source diagnostic locator",
                        PrepareStage::SourceValidation,
                        budget,
                    )?,
                    Some(expectation.fingerprint()),
                    None,
                ));
            }
            LocatorResolution::Missing => {
                return Err(RunnerFailure::source(
                    "PREPARE_SOURCE_MISSING",
                    "declared source does not exist",
                    budgeted_source_locator_clone(
                        expectation.locator(),
                        "prepare source diagnostic locator",
                        PrepareStage::SourceValidation,
                        budget,
                    )?,
                    Some(expectation.fingerprint()),
                    None,
                ));
            }
            LocatorResolution::Invalid => {
                return Err(RunnerFailure::source(
                    "PREPARE_SOURCE_INVALID",
                    "declared source locator is invalid for the loaded containment graph",
                    budgeted_source_locator_clone(
                        expectation.locator(),
                        "prepare source diagnostic locator",
                        PrepareStage::SourceValidation,
                        budget,
                    )?,
                    Some(expectation.fingerprint()),
                    None,
                ));
            }
        };
        let actual = match catalog.fingerprint(source) {
            Ok(actual) => actual,
            Err(error) => {
                return Err(RunnerFailure::source(
                    "PREPARE_SOURCE_VALIDATION_FAILED",
                    error.to_string(),
                    budgeted_source_locator_clone(
                        expectation.locator(),
                        "prepare source diagnostic locator",
                        PrepareStage::SourceValidation,
                        budget,
                    )?,
                    Some(expectation.fingerprint()),
                    None,
                ));
            }
        };
        if actual != expectation.fingerprint() {
            return Err(RunnerFailure::source(
                "PREPARE_SOURCE_MISMATCH",
                "declared source fingerprint does not match the workspace snapshot",
                budgeted_source_locator_clone(
                    expectation.locator(),
                    "prepare source diagnostic locator",
                    PrepareStage::SourceValidation,
                    budget,
                )?,
                Some(expectation.fingerprint()),
                Some(actual),
            ));
        }
    }
    PhysicalDependencyProofSet::observe(catalog, budget)
        .map_err(|error| source_proof_failure(catalog, error, budget))
}

fn predict_publication_roots(
    catalog: &SourceCatalog,
    groups: &SourceCandidateSets<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<SourceId>, RunnerFailure> {
    let group_count = groups.checked_len().ok_or_else(|| {
        RunnerFailure::new(
            None,
            PrepareStage::ArtifactDeclaration,
            "PREPARE_ALLOCATION_OVERFLOW",
            "publication root count overflow",
        )
    })?;
    let mut roots = budgeted_vec(
        group_count,
        "prepare publication roots",
        PrepareStage::ArtifactDeclaration,
        budget,
    )?;
    for source in groups.sources() {
        let mut current = source;
        loop {
            let descriptor = catalog
                .resolve(current)
                .map_err(|error| catalog_failure(PrepareStage::ArtifactDeclaration, error))?;
            match descriptor.location_kind() {
                SourceLocationKind::Root | SourceLocationKind::Companion => break,
                SourceLocationKind::ArchiveMember
                | SourceLocationKind::WebFileMember
                | SourceLocationKind::BundleMember
                | SourceLocationKind::Sidecar => {
                    current = descriptor.parent().ok_or_else(|| {
                        RunnerFailure::new(
                            None,
                            PrepareStage::ArtifactDeclaration,
                            "PREPARE_PUBLICATION_ROOT_REJECTED",
                            format!("contained source {current:?} has no parent"),
                        )
                    })?;
                }
            }
        }
        roots.push(current);
    }
    roots.sort_unstable();
    roots.dedup();
    Ok(roots)
}

#[derive(Debug)]
struct OutputDestinationSpec {
    source: SourceId,
    target: PathBuf,
    expectation: DestinationExpectation,
}

#[derive(Debug)]
struct PendingOutputSpec {
    name: LogicalArtifactName,
    destination: OutputDestinationSpec,
}

#[derive(Debug)]
struct DeclaredOutputSpec {
    slot: OutputSlot,
    destination: OutputDestinationSpec,
}

#[derive(Clone, Copy)]
struct PublicationBinding<'output> {
    output: &'output LogicalArtifactName,
    destination: &'output OutputDestinationSpec,
}

fn build_output_specs(
    catalog: &SourceCatalog,
    roots: &[SourceId],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<PendingOutputSpec>, RunnerFailure> {
    let mut outputs = budgeted_vec(
        roots.len(),
        "prepare output specifications",
        PrepareStage::ArtifactDeclaration,
        budget,
    )?;
    for (ordinal, source) in roots.iter().enumerate() {
        let name = budgeted_output_name(ordinal, budget)?;
        outputs.push(PendingOutputSpec {
            name,
            destination: build_output_destination(catalog, *source, budget)?,
        });
    }
    Ok(outputs)
}

fn build_declared_output_spec(
    catalog: &SourceCatalog,
    source: SourceId,
    slot: OutputSlot,
    budget: &mut AssetLoadBudget,
) -> Result<DeclaredOutputSpec, RunnerFailure> {
    Ok(DeclaredOutputSpec {
        slot,
        destination: build_output_destination(catalog, source, budget)?,
    })
}

fn build_output_destination(
    catalog: &SourceCatalog,
    source: SourceId,
    budget: &mut AssetLoadBudget,
) -> Result<OutputDestinationSpec, RunnerFailure> {
    let (target, expectation) =
        match catalog
            .physical_origin_option(source)
            .map_err(|error| catalog_failure(PrepareStage::DestinationValidation, error))?
        {
            Some(origin) => (
                budgeted_path_copy(
                    origin.path(),
                    "prepare publication target",
                    PrepareStage::DestinationValidation,
                    budget,
                )?,
                DestinationExpectation::Existing(catalog.fingerprint(source).map_err(|error| {
                    catalog_failure(PrepareStage::DestinationValidation, error)
                })?),
            ),
            None => {
                let descriptor = catalog
                    .resolve(source)
                    .map_err(|error| catalog_failure(PrepareStage::DestinationValidation, error))?;
                if descriptor.location_kind() != SourceLocationKind::Companion {
                    return Err(RunnerFailure::new(
                        None,
                        PrepareStage::DestinationValidation,
                        "PREPARE_PUBLICATION_TARGET_MISSING",
                        format!("publication root {source:?} has no physical origin"),
                    ));
                }
                let parent = descriptor.parent().ok_or_else(|| {
                    RunnerFailure::new(
                        None,
                        PrepareStage::DestinationValidation,
                        "PREPARE_COMPANION_PARENT_MISSING",
                        format!("companion source {source:?} has no parent"),
                    )
                })?;
                let parent_origin = catalog
                    .physical_origin(parent)
                    .map_err(|error| catalog_failure(PrepareStage::DestinationValidation, error))?;
                let directory = parent_origin.path().parent().ok_or_else(|| {
                    RunnerFailure::new(
                        None,
                        PrepareStage::DestinationValidation,
                        "PREPARE_COMPANION_DIRECTORY_MISSING",
                        format!("physical parent {parent:?} has no containing directory"),
                    )
                })?;
                let locator = catalog
                    .source_locator(source)
                    .map_err(|error| catalog_failure(PrepareStage::DestinationValidation, error))?;
                let member = locator.members().last().ok_or_else(|| {
                    RunnerFailure::new(
                        None,
                        PrepareStage::DestinationValidation,
                        "PREPARE_COMPANION_NAME_MISSING",
                        format!("companion source {source:?} has no locator member"),
                    )
                })?;
                (
                    budgeted_path_join(
                        directory,
                        Path::new(member.member().name()),
                        "prepare companion publication target",
                        PrepareStage::DestinationValidation,
                        budget,
                    )?,
                    DestinationExpectation::Absent,
                )
            }
        };
    Ok(OutputDestinationSpec {
        source,
        target,
        expectation,
    })
}

fn build_publication_bindings<'output>(
    artifacts: &'output PreparedArtifactSet,
    outputs: &'output [DeclaredOutputSpec],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<PublicationBinding<'output>>, RunnerFailure> {
    let mut bindings = budgeted_vec(
        outputs.len(),
        "prepare publication bindings",
        PrepareStage::DestinationValidation,
        budget,
    )?;
    for spec in outputs {
        let output = artifacts
            .output(spec.slot)
            .map_err(|error| artifact_failure(PrepareStage::DestinationValidation, error))?;
        bindings.push(PublicationBinding {
            output: output.name(),
            destination: &spec.destination,
        });
    }
    bindings.sort_unstable_by(|left, right| left.output.cmp(right.output));
    Ok(bindings)
}

fn bind_publication_roots(
    batch: &mut unity_asset_write::artifact::ArtifactBatch<'_, '_>,
    graph: &PreparedArtifactGraph,
    predicted: &[SourceId],
    outputs: &[DeclaredOutputSpec],
    self_bound: &[SelfBoundPublicationRoot],
) -> Result<(), RunnerFailure> {
    let expected_count = predicted
        .len()
        .checked_add(self_bound.len())
        .ok_or_else(|| {
            RunnerFailure::new(
                None,
                PrepareStage::ArtifactEncoding,
                "PREPARE_PUBLICATION_ROOT_MISMATCH",
                "publication root count overflow",
            )
        })?;
    if graph.publication_roots().len() != expected_count || outputs.len() != expected_count {
        return Err(RunnerFailure::new(
            None,
            PrepareStage::ArtifactEncoding,
            "PREPARE_PUBLICATION_ROOT_MISMATCH",
            "predicted and encoded publication root counts differ",
        ));
    }
    let mut bound_regular = 0_usize;
    let mut observed_self_bound = 0_usize;
    for binding in graph.publication_roots() {
        if let Ok(index) = predicted.binary_search(&binding.source()) {
            let output = outputs.get(index).ok_or_else(|| {
                RunnerFailure::new(
                    None,
                    PrepareStage::ArtifactEncoding,
                    "PREPARE_PUBLICATION_ROOT_MISMATCH",
                    "predicted publication root lost its declared output",
                )
            })?;
            if output.destination.source != binding.source() {
                return Err(RunnerFailure::new(
                    None,
                    PrepareStage::ArtifactEncoding,
                    "PREPARE_PUBLICATION_ROOT_MISMATCH",
                    "declared output belongs to a different publication root",
                ));
            }
            batch
                .bind_output(output.slot, binding.artifact())
                .map_err(|error| artifact_failure(PrepareStage::ArtifactEncoding, error))?;
            bound_regular += 1;
            continue;
        }
        let Ok(index) = self_bound.binary_search_by_key(&binding.source(), |root| root.source)
        else {
            return Err(RunnerFailure::new(
                None,
                PrepareStage::ArtifactEncoding,
                "PREPARE_PUBLICATION_ROOT_MISMATCH",
                format!("unexpected encoded publication root {:?}", binding.source()),
            ));
        };
        if self_bound[index].artifact != binding.artifact() {
            return Err(RunnerFailure::new(
                None,
                PrepareStage::ArtifactEncoding,
                "PREPARE_PUBLICATION_ROOT_MISMATCH",
                format!(
                    "self-bound publication root {:?} references the wrong artifact",
                    binding.source()
                ),
            ));
        }
        observed_self_bound += 1;
    }
    if bound_regular != predicted.len() || observed_self_bound != self_bound.len() {
        return Err(RunnerFailure::new(
            None,
            PrepareStage::ArtifactEncoding,
            "PREPARE_PUBLICATION_ROOT_MISMATCH",
            "one or more publication roots were not encoded",
        ));
    }
    Ok(())
}

fn build_candidate_catalog(
    base: &SourceCatalog,
    graph: &PreparedArtifactGraph,
    artifacts: &unity_asset_write::artifact::PreparedArtifactSet,
    budget: &mut AssetLoadBudget,
) -> Result<SourceCatalog, RunnerFailure> {
    let mut changes = budgeted_vec(
        graph.bindings().len(),
        "prepare physical domain changes",
        PrepareStage::PreparedView,
        budget,
    )?;
    for binding in graph.bindings() {
        let artifact = artifacts
            .artifact(binding.artifact())
            .map_err(|error| artifact_failure(PrepareStage::PreparedView, error))?;
        changes.push(PhysicalDomainChange::new(
            binding.source(),
            SourceFingerprint::new(binding.source().kind(), artifact.digest()),
        ));
    }
    changes.sort_unstable_by_key(PhysicalDomainChange::source);
    let batch = base
        .prepare_physical_domain_rewrite_batch(&changes, budget)
        .map_err(|error| catalog_failure(PrepareStage::PreparedView, error))?;
    let mut transaction = base
        .begin_transaction(budget)
        .map_err(|error| catalog_failure(PrepareStage::PreparedView, error))?;
    transaction
        .rewrite_physical_domains(batch, budget)
        .map_err(|error| catalog_failure(PrepareStage::PreparedView, error))?;
    transaction
        .commit(budget)
        .map_err(|error| catalog_failure(PrepareStage::PreparedView, error))
}

fn build_source_bindings_and_reports(
    base: &SourceCatalog,
    catalog: &SourceCatalog,
    graph: &PreparedArtifactGraph,
    leaves: &[(SourceId, ArtifactHandle)],
    artifacts: &unity_asset_write::artifact::PreparedArtifactSet,
    budget: &mut AssetLoadBudget,
) -> Result<(Vec<PreparedSourceBinding>, Vec<PreparedSourceReport>), RunnerFailure> {
    let mut bindings = budgeted_vec(
        graph.bindings().len(),
        "prepare source bindings",
        PrepareStage::PreparedView,
        budget,
    )?;
    let mut reports = budgeted_vec(
        graph.bindings().len(),
        "prepare source reports",
        PrepareStage::PreparedView,
        budget,
    )?;
    for binding in graph.bindings() {
        let source = binding.source();
        let artifact = artifacts
            .artifact(binding.artifact())
            .map_err(|error| artifact_failure(PrepareStage::PreparedView, error))?;
        let fingerprint = SourceFingerprint::new(source.kind(), artifact.digest());
        let publication_root = graph
            .publication_roots()
            .binary_search_by_key(&source, |candidate| candidate.source())
            .is_ok();
        let prepared = PreparedSourceBinding::new(source, fingerprint, binding.artifact());
        bindings.push(if publication_root {
            prepared
        } else {
            prepared.nested()
        });
        let logical_changed = leaves
            .binary_search_by_key(&source, |(candidate, _)| *candidate)
            .is_ok();
        reports.push(PreparedSourceReport {
            source_id: source,
            locator: budgeted_source_locator_clone(
                catalog
                    .source_locator(source)
                    .map_err(|error| catalog_failure(PrepareStage::PreparedView, error))?,
                "prepare source report locator",
                PrepareStage::PreparedView,
                budget,
            )?,
            physical_domain_owner: catalog
                .physical_domain_owner(source)
                .map_err(|error| catalog_failure(PrepareStage::PreparedView, error))?,
            base_fingerprint: base
                .contains(source)
                .then(|| base.fingerprint(source))
                .transpose()
                .map_err(|error| catalog_failure(PrepareStage::PreparedView, error))?,
            prepared_fingerprint: fingerprint,
            artifact_digest: artifact.digest(),
            artifact_bytes: artifact.len(),
            logical_changed_bytes: if logical_changed { artifact.len() } else { 0 },
            physical_rewrite_bytes: if publication_root { artifact.len() } else { 0 },
            publication_root,
        });
    }
    Ok((bindings, reports))
}

fn inspect_runner_budget<T>(
    batch: &mut unity_asset_write::artifact::ArtifactBatch<'_, '_>,
    inspect: impl FnOnce(&mut AssetLoadBudget) -> Result<T, RunnerFailure>,
) -> Result<T, RunnerFailure> {
    let mut outcome = None;
    batch
        .inspect_with_budget(|budget| {
            outcome = Some(inspect(budget));
            Ok(())
        })
        .map_err(|error| artifact_failure(PrepareStage::ArtifactEncoding, error))?;
    outcome.expect("artifact inspection closure always records its result")
}

fn retain_artifact_set(
    artifacts: PreparedArtifactSet,
    budget: &mut AssetLoadBudget,
) -> Result<Arc<PreparedArtifactSet>, RunnerFailure> {
    let retained = arc_value_allocation_bytes::<PreparedArtifactSet>().map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ArtifactEncoding,
            "PREPARE_ARTIFACT_SET_RETENTION_REJECTED",
            error.to_string(),
        )
    })?;
    budget.check_bytes(retained).map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ArtifactEncoding,
            "PREPARE_ARTIFACT_SET_RETENTION_REJECTED",
            error.to_string(),
        )
    })?;
    budget.consume_bytes(retained).map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ArtifactEncoding,
            "PREPARE_ARTIFACT_SET_RETENTION_REJECTED",
            error.to_string(),
        )
    })?;
    Ok(Arc::new(artifacts))
}

fn budgeted_vec<T>(
    capacity: usize,
    _resource: &'static str,
    stage: PrepareStage,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, RunnerFailure> {
    let entries = u64::try_from(capacity).map_err(|_| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "collection capacity does not fit the load-budget ledger",
        )
    })?;
    let minimum = vec_allocation_bytes::<T>(capacity).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            error.to_string(),
        )
    })?;
    budget.check_entries(entries).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    budget.check_bytes(minimum).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    let mut values = Vec::new();
    values.try_reserve_exact(capacity).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    let actual = vec_allocation_bytes::<T>(values.capacity()).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            error.to_string(),
        )
    })?;
    budget.check_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    budget.consume_entries(entries).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    budget.consume_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    Ok(values)
}

fn budgeted_vec_insert<T>(
    values: &mut Vec<T>,
    index: usize,
    value: T,
    _resource: &'static str,
    stage: PrepareStage,
    budget: &mut AssetLoadBudget,
) -> Result<(), RunnerFailure> {
    budget.check_entries(1).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    if values.len() == values.capacity() {
        let required = values.len().checked_add(1).ok_or_else(|| {
            RunnerFailure::new(
                None,
                stage,
                "PREPARE_ALLOCATION_OVERFLOW",
                "collection capacity overflow",
            )
        })?;
        let before = vec_allocation_bytes::<T>(values.capacity()).map_err(|error| {
            RunnerFailure::new(
                None,
                stage,
                "PREPARE_ALLOCATION_OVERFLOW",
                error.to_string(),
            )
        })?;
        let minimum = vec_allocation_bytes::<T>(required)
            .map_err(|error| {
                RunnerFailure::new(
                    None,
                    stage,
                    "PREPARE_ALLOCATION_OVERFLOW",
                    error.to_string(),
                )
            })?
            .checked_sub(before)
            .ok_or_else(|| {
                RunnerFailure::new(
                    None,
                    stage,
                    "PREPARE_ALLOCATION_OVERFLOW",
                    "collection allocation delta underflow",
                )
            })?;
        budget.check_bytes(minimum).map_err(|error| {
            RunnerFailure::new(
                None,
                stage,
                "PREPARE_ALLOCATION_REJECTED",
                error.to_string(),
            )
        })?;
        values.try_reserve_exact(1).map_err(|error| {
            RunnerFailure::new(
                None,
                stage,
                "PREPARE_ALLOCATION_REJECTED",
                error.to_string(),
            )
        })?;
        let actual = vec_allocation_bytes::<T>(values.capacity())
            .map_err(|error| {
                RunnerFailure::new(
                    None,
                    stage,
                    "PREPARE_ALLOCATION_OVERFLOW",
                    error.to_string(),
                )
            })?
            .checked_sub(before)
            .ok_or_else(|| {
                RunnerFailure::new(
                    None,
                    stage,
                    "PREPARE_ALLOCATION_OVERFLOW",
                    "collection allocation delta underflow",
                )
            })?;
        budget.check_bytes(actual).map_err(|error| {
            RunnerFailure::new(
                None,
                stage,
                "PREPARE_ALLOCATION_REJECTED",
                error.to_string(),
            )
        })?;
        budget.consume_bytes(actual).map_err(|error| {
            RunnerFailure::new(
                None,
                stage,
                "PREPARE_ALLOCATION_REJECTED",
                error.to_string(),
            )
        })?;
    }
    budget.consume_entries(1).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    values.insert(index, value);
    Ok(())
}

fn budgeted_source_locator_clone(
    value: &SourceLocator,
    _resource: &'static str,
    stage: PrepareStage,
    budget: &mut AssetLoadBudget,
) -> Result<SourceLocator, RunnerFailure> {
    let minimum = value.retained_clone_bytes().ok_or_else(|| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "source locator clone allocation overflow",
        )
    })?;
    let minimum = u64::try_from(minimum).map_err(|_| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "source locator clone allocation does not fit the load-budget ledger",
        )
    })?;
    budget.check_bytes(minimum).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    let cloned = value.clone();
    let actual = cloned.retained_clone_bytes().ok_or_else(|| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "source locator clone allocation overflow",
        )
    })?;
    let actual = u64::try_from(actual).map_err(|_| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "source locator clone allocation does not fit the load-budget ledger",
        )
    })?;
    budget.check_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    budget.consume_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    Ok(cloned)
}

fn budgeted_object_address_clone(
    value: &ObjectAddress,
    _resource: &'static str,
    stage: PrepareStage,
    budget: &mut AssetLoadBudget,
) -> Result<ObjectAddress, RunnerFailure> {
    let minimum = value.retained_clone_bytes().ok_or_else(|| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "object address clone allocation overflow",
        )
    })?;
    let minimum = u64::try_from(minimum).map_err(|_| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "object address clone allocation does not fit the load-budget ledger",
        )
    })?;
    budget.check_bytes(minimum).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    let cloned = value.clone();
    let actual = cloned.retained_clone_bytes().ok_or_else(|| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "object address clone allocation overflow",
        )
    })?;
    let actual = u64::try_from(actual).map_err(|_| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "object address clone allocation does not fit the load-budget ledger",
        )
    })?;
    budget.check_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    budget.consume_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    Ok(cloned)
}

fn budgeted_field_path_clone(
    value: &FieldPath,
    _resource: &'static str,
    stage: PrepareStage,
    budget: &mut AssetLoadBudget,
) -> Result<FieldPath, RunnerFailure> {
    let minimum = value.retained_clone_bytes().ok_or_else(|| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "field path clone allocation overflow",
        )
    })?;
    let minimum = u64::try_from(minimum).map_err(|_| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "field path clone allocation does not fit the load-budget ledger",
        )
    })?;
    budget.check_bytes(minimum).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    let cloned = value.clone();
    let actual = cloned.retained_clone_bytes().ok_or_else(|| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "field path clone allocation overflow",
        )
    })?;
    let actual = u64::try_from(actual).map_err(|_| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "field path clone allocation does not fit the load-budget ledger",
        )
    })?;
    budget.check_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    budget.consume_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    Ok(cloned)
}

fn budgeted_output_name(
    ordinal: usize,
    budget: &mut AssetLoadBudget,
) -> Result<LogicalArtifactName, RunnerFailure> {
    const OUTPUT_NAME_BYTES: usize = "output/".len() + 16;
    let minimum = OUTPUT_NAME_BYTES.checked_mul(2).ok_or_else(|| {
        RunnerFailure::new(
            None,
            PrepareStage::ArtifactDeclaration,
            "PREPARE_ALLOCATION_OVERFLOW",
            "logical output name allocation overflow",
        )
    })?;
    let minimum = u64::try_from(minimum).map_err(|_| {
        RunnerFailure::new(
            None,
            PrepareStage::ArtifactDeclaration,
            "PREPARE_ALLOCATION_OVERFLOW",
            "logical output name allocation does not fit the load-budget ledger",
        )
    })?;
    budget.check_bytes(minimum).map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ArtifactDeclaration,
            "PREPARE_OUTPUT_NAME_REJECTED",
            error.to_string(),
        )
    })?;
    let mut raw = String::new();
    raw.try_reserve_exact(OUTPUT_NAME_BYTES).map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ArtifactDeclaration,
            "PREPARE_OUTPUT_NAME_REJECTED",
            error.to_string(),
        )
    })?;
    write!(&mut raw, "output/{ordinal:016x}").map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ArtifactDeclaration,
            "PREPARE_OUTPUT_NAME_REJECTED",
            error.to_string(),
        )
    })?;
    let name = LogicalArtifactName::try_from(raw).map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ArtifactDeclaration,
            "PREPARE_OUTPUT_NAME_REJECTED",
            error.to_string(),
        )
    })?;
    let actual = name.retained_bytes().map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ArtifactDeclaration,
            "PREPARE_OUTPUT_NAME_REJECTED",
            error.to_string(),
        )
    })?;
    budget.check_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ArtifactDeclaration,
            "PREPARE_OUTPUT_NAME_REJECTED",
            error.to_string(),
        )
    })?;
    budget.consume_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            PrepareStage::ArtifactDeclaration,
            "PREPARE_OUTPUT_NAME_REJECTED",
            error.to_string(),
        )
    })?;
    Ok(name)
}

fn budgeted_path_copy(
    value: &Path,
    resource: &'static str,
    stage: PrepareStage,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, RunnerFailure> {
    budgeted_os_string(value.as_os_str(), None, resource, stage, budget).map(PathBuf::from)
}

fn budgeted_path_join(
    base: &Path,
    tail: &Path,
    resource: &'static str,
    stage: PrepareStage,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, RunnerFailure> {
    budgeted_os_string(base.as_os_str(), Some(tail), resource, stage, budget).map(PathBuf::from)
}

fn budgeted_os_string(
    base: &OsStr,
    tail: Option<&Path>,
    _resource: &'static str,
    stage: PrepareStage,
    budget: &mut AssetLoadBudget,
) -> Result<OsString, RunnerFailure> {
    let requested = base
        .len()
        .checked_add(tail.map_or(0, |path| path.as_os_str().len().saturating_add(1)))
        .ok_or_else(|| {
            RunnerFailure::new(
                None,
                stage,
                "PREPARE_ALLOCATION_OVERFLOW",
                "publication path allocation overflow",
            )
        })?;
    let requested_u64 = u64::try_from(requested).map_err(|_| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "publication path allocation does not fit the load-budget ledger",
        )
    })?;
    budget.check_bytes(requested_u64).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    let mut value = OsString::new();
    value.try_reserve_exact(requested).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    value.push(base);
    if let Some(tail) = tail {
        let mut path = PathBuf::from(value);
        path.push(tail);
        value = path.into_os_string();
    }
    let actual = u64::try_from(value.capacity()).map_err(|_| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_OVERFLOW",
            "publication path allocation does not fit the load-budget ledger",
        )
    })?;
    budget.check_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    budget.consume_bytes(actual).map_err(|error| {
        RunnerFailure::new(
            None,
            stage,
            "PREPARE_ALLOCATION_REJECTED",
            error.to_string(),
        )
    })?;
    Ok(value)
}

fn destination_failure(
    error: DestinationProofError,
    code: &'static str,
    outputs: &[PublicationBinding<'_>],
    catalog: &SourceCatalog,
    budget: &mut AssetLoadBudget,
) -> RunnerFailure {
    let output = destination_error_output(&error);
    let (expected, actual) = destination_error_fingerprints(&error);
    let mut failure = RunnerFailure::new(
        None,
        PrepareStage::DestinationValidation,
        code,
        error.to_string(),
    );
    failure.0.expected = expected;
    failure.0.actual = actual;
    if let Some(binding) = output.and_then(|output| outputs.get(output)) {
        failure.0.source = catalog
            .source_locator(binding.destination.source)
            .ok()
            .and_then(|locator| {
                budgeted_source_locator_clone(
                    locator,
                    "prepare destination diagnostic locator",
                    PrepareStage::DestinationValidation,
                    budget,
                )
                .ok()
            });
    }
    failure
}

fn source_proof_failure(
    catalog: &SourceCatalog,
    error: PhysicalDependencyProofError,
    budget: &mut AssetLoadBudget,
) -> RunnerFailure {
    let source = error.source_id();
    let expected = error.expected_fingerprint();
    let actual = error.actual_fingerprint();
    let code = if actual.is_some() {
        "PREPARE_SOURCE_CHANGED"
    } else {
        "PREPARE_SOURCE_REVALIDATION_FAILED"
    };
    let mut failure = RunnerFailure::new(
        None,
        PrepareStage::SourceValidation,
        code,
        error.to_string(),
    );
    failure.0.expected = expected;
    failure.0.actual = actual;
    failure.0.source = source
        .and_then(|source| catalog.source_locator(source).ok())
        .and_then(|locator| {
            budgeted_source_locator_clone(
                locator,
                "prepare source proof diagnostic locator",
                PrepareStage::SourceValidation,
                budget,
            )
            .ok()
        });
    failure
}

fn destination_error_output(error: &DestinationProofError) -> Option<usize> {
    match error {
        DestinationProofError::OutputNameMismatch { output, .. }
        | DestinationProofError::NonAbsoluteTarget { output }
        | DestinationProofError::UnsupportedTargetEncoding { output }
        | DestinationProofError::TargetEscapesRoot { output }
        | DestinationProofError::InvalidTargetState { output, .. }
        | DestinationProofError::InvalidParentState { output, .. }
        | DestinationProofError::InvalidAbsentTarget { output }
        | DestinationProofError::ObservationMismatch { output, .. }
        | DestinationProofError::FileIdentityChanged { output, .. }
        | DestinationProofError::ParentIdentityChanged { output }
        | DestinationProofError::PathComponentChanged { output, .. }
        | DestinationProofError::Io { output, .. }
        | DestinationProofError::Catalog { output, .. } => Some(*output),
        DestinationProofError::DuplicateTarget { first_output, .. }
        | DestinationProofError::PortableTargetCollision { first_output, .. } => {
            Some(*first_output)
        }
        DestinationProofError::Budget(_)
        | DestinationProofError::ArithmeticOverflow { .. }
        | DestinationProofError::Allocation { .. }
        | DestinationProofError::OutputCountMismatch { .. }
        | DestinationProofError::DuplicateOutput { .. }
        | DestinationProofError::UnsupportedArtifactFormat => None,
    }
}

fn destination_error_fingerprints(
    error: &DestinationProofError,
) -> (Option<SourceFingerprint>, Option<SourceFingerprint>) {
    match error {
        DestinationProofError::ObservationMismatch {
            expected, actual, ..
        } => (
            destination_fingerprint(*expected),
            destination_fingerprint(*actual),
        ),
        DestinationProofError::FileIdentityChanged {
            expected_fingerprint,
            ..
        } => (Some(*expected_fingerprint), Some(*expected_fingerprint)),
        DestinationProofError::InvalidTargetState { actual, .. }
        | DestinationProofError::InvalidParentState { actual, .. }
        | DestinationProofError::PathComponentChanged { actual, .. } => {
            (None, destination_fingerprint(*actual))
        }
        DestinationProofError::Catalog { source, .. } => match source.as_ref() {
            CatalogError::VerifiedFingerprintMismatch { expected, actual } => {
                (Some(*expected), Some(*actual))
            }
            _ => (None, None),
        },
        _ => (None, None),
    }
}

const fn destination_fingerprint(state: DestinationState) -> Option<SourceFingerprint> {
    match state {
        DestinationState::Existing(fingerprint) => Some(fingerprint),
        DestinationState::Absent
        | DestinationState::Directory
        | DestinationState::SymbolicLink
        | DestinationState::Other => None,
    }
}

fn mutation_field_path(action: &GenericMutation) -> Option<&FieldPath> {
    match action {
        GenericMutation::FieldReplace { path, .. }
        | GenericMutation::ReferenceReplace { path, .. }
        | GenericMutation::ResourceReplace { path, .. }
        | GenericMutation::SequenceEdit { path, .. } => Some(path),
        GenericMutation::SchemaReplace { .. } | GenericMutation::UnsafeRawReplace { .. } => None,
    }
}

fn artifact_failure(stage: PrepareStage, error: ArtifactBuildError) -> RunnerFailure {
    RunnerFailure::new(None, stage, "PREPARE_ARTIFACT_REJECTED", error.to_string())
}

fn artifact_prepare_failure(error: ArtifactBuildError) -> RunnerFailure {
    artifact_failure(artifact_build_stage(error.failure_phase()), error)
}

const fn artifact_build_stage(phase: ArtifactBuildFailurePhase) -> PrepareStage {
    match phase {
        ArtifactBuildFailurePhase::Encoding => PrepareStage::ArtifactEncoding,
        ArtifactBuildFailurePhase::IndependentReparse => PrepareStage::IndependentReparse,
        _ => PrepareStage::ArtifactEncoding,
    }
}

fn catalog_failure(stage: PrepareStage, error: impl ToString) -> RunnerFailure {
    RunnerFailure::new(None, stage, "PREPARE_CATALOG_REJECTED", error.to_string())
}

#[derive(Debug)]
struct RunnerFailure(Box<RunnerFailureData>);

#[derive(Debug)]
struct RunnerFailureData {
    ordinal: Option<u32>,
    stage: PrepareStage,
    code: &'static str,
    message: String,
    source: Option<SourceLocator>,
    expected: Option<SourceFingerprint>,
    actual: Option<SourceFingerprint>,
    address: Option<ObjectAddress>,
    field: Option<FieldPath>,
}

impl RunnerFailure {
    fn new(
        ordinal: Option<u32>,
        stage: PrepareStage,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self(Box::new(RunnerFailureData {
            ordinal,
            stage,
            code,
            message: message.into(),
            source: None,
            expected: None,
            actual: None,
            address: None,
            field: None,
        }))
    }

    fn source(
        code: &'static str,
        message: impl Into<String>,
        source: SourceLocator,
        expected: Option<SourceFingerprint>,
        actual: Option<SourceFingerprint>,
    ) -> Self {
        let mut failure = Self::new(None, PrepareStage::SourceValidation, code, message);
        failure.0.source = Some(source);
        failure.0.expected = expected;
        failure.0.actual = actual;
        failure
    }

    fn operation(
        ordinal: u32,
        code: &'static str,
        message: impl Into<String>,
        address: ObjectAddress,
        field: Option<FieldPath>,
    ) -> Self {
        Self::operation_at(
            ordinal,
            PrepareStage::Mutation,
            code,
            message,
            address,
            field,
        )
    }

    fn address(
        ordinal: u32,
        code: &'static str,
        message: impl Into<String>,
        address: ObjectAddress,
    ) -> Self {
        Self::operation_at(
            ordinal,
            PrepareStage::AddressResolution,
            code,
            message,
            address,
            None,
        )
    }

    fn operation_at(
        ordinal: u32,
        stage: PrepareStage,
        code: &'static str,
        message: impl Into<String>,
        address: ObjectAddress,
        field: Option<FieldPath>,
    ) -> Self {
        let mut failure = Self::new(Some(ordinal), stage, code, message);
        failure.0.address = Some(address);
        failure.0.field = field;
        failure
    }

    fn with_operation_context(
        mut self,
        ordinal: u32,
        stage: PrepareStage,
        code: &'static str,
        address: ObjectAddress,
        field: Option<FieldPath>,
    ) -> Self {
        self.0.ordinal = Some(ordinal);
        self.0.stage = stage;
        self.0.code = code;
        self.0.address = Some(address);
        self.0.field = field;
        self
    }
}

fn reject(
    snapshot: &WorkspaceSnapshot,
    plan_digest: Option<DigestV1>,
    failure: RunnerFailure,
) -> PrepareError {
    let RunnerFailure(failure) = failure;
    let RunnerFailureData {
        ordinal,
        stage,
        code,
        mut message,
        source,
        expected,
        actual,
        address,
        field,
    } = *failure;
    if message.len() > MAX_RUNNER_DIAGNOSTIC_BYTES {
        let mut end = MAX_RUNNER_DIAGNOSTIC_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    let mut diagnostic = Diagnostic::new(DiagnosticSeverity::Error, code, message)
        .expect("runner diagnostic constants satisfy the diagnostic contract");
    if let Some(address) = address {
        diagnostic = diagnostic.at_address(address);
    }
    if let Some(field) = field {
        diagnostic = diagnostic.at_field(field);
    }
    PrepareError {
        report: PrepareFailureReport {
            version: PREPARE_REPORT_VERSION,
            workspace_id: snapshot.workspace_id(),
            observed_revision: snapshot.revision(),
            plan_digest,
            diagnostics: vec![PrepareDiagnostic {
                ordinal,
                stage,
                diagnostic,
                source,
                expected_fingerprint: expected,
                actual_fingerprint: actual,
            }],
        },
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;

    use unity_asset_core::{AssetLoadLimits, SourceAlias, SourceKind};
    use unity_asset_write::artifact::{ArtifactBatchDeclaration, ArtifactLimits};

    use super::*;
    use crate::workspace::{FieldGuard, MutationValue, SourceExpectation, SourceOpenRequest};

    const OLD_YAML: &str = "%YAML 1.1\n---\nvalue: old\n";
    const NEW_YAML: &str = "%YAML 1.1\n---\nvalue: new\n";

    fn one_yaml_artifact(name: &str) -> unity_asset_write::artifact::PreparedArtifactSet {
        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
        let slot = declaration
            .declare_output(LogicalArtifactName::new(name).unwrap())
            .unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let mut writer = batch.yaml_writer().unwrap();
        writeln!(writer, "---\nvalue: prepared").unwrap();
        let artifact = batch.prepare_yaml_writer(writer).unwrap();
        batch.bind_output(slot, artifact).unwrap();
        batch.finish().unwrap()
    }

    #[test]
    fn retained_artifact_set_arc_has_an_exact_atomic_budget_boundary() {
        let required = arc_value_allocation_bytes::<PreparedArtifactSet>().unwrap();
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: required,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let retained = retain_artifact_set(one_yaml_artifact("exact"), &mut exact).unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(exact.usage().bytes, required);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: required - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error =
            retain_artifact_set(one_yaml_artifact("one-short"), &mut one_short).unwrap_err();
        assert_eq!(error.0.stage, PrepareStage::ArtifactEncoding);
        assert_eq!(error.0.code, "PREPARE_ARTIFACT_SET_RETENTION_REJECTED");
        assert_eq!(one_short.usage().bytes, 0);
    }

    #[test]
    fn resource_manifest_index_classifies_each_future_resource_operation_once() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.asset");
        let second_path = directory.path().join("second.asset");
        fs::write(&first_path, OLD_YAML).unwrap();
        fs::write(&second_path, OLD_YAML).unwrap();
        let mut workspace = AssetWorkspace::new().unwrap();
        for (path, alias) in [(&first_path, "first.asset"), (&second_path, "second.asset")] {
            workspace
                .load_source(
                    SourceOpenRequest::new(path, SourceAlias::new(alias).unwrap())
                        .with_kind_hint(SourceKind::Yaml),
                    &mut AssetLoadBudget::default(),
                )
                .unwrap();
        }
        let snapshot = workspace.snapshot();
        let first_locator = SourceLocator::path("first.asset").unwrap();
        let second_locator = SourceLocator::path("second.asset").unwrap();
        let first_address = ObjectAddress::yaml(first_locator.clone(), "1").unwrap();
        let second_address = ObjectAddress::yaml(second_locator.clone(), "1").unwrap();
        let field = FieldPath::root().push_field("m_StreamData").unwrap();
        let digest = DigestV1::hash_bytes(b"resource-index");
        let guard = FieldGuard::new(digest, digest);
        let payload = PlanPayload::new(b"payload".to_vec());
        let plan = MutationPlan::new(
            snapshot.revision(),
            vec![
                SourceExpectation::new(
                    first_locator,
                    SourceFingerprint::from_bytes(SourceKind::Yaml, OLD_YAML.as_bytes()),
                ),
                SourceExpectation::new(
                    second_locator,
                    SourceFingerprint::from_bytes(SourceKind::Yaml, OLD_YAML.as_bytes()),
                ),
            ],
            vec![payload.clone()],
            vec![
                GenericMutation::ResourceReplace {
                    target: first_address.clone(),
                    path: field.clone(),
                    guard,
                    payload: payload.digest(),
                },
                GenericMutation::FieldReplace {
                    target: first_address.clone(),
                    path: FieldPath::root().push_field("value").unwrap(),
                    guard,
                    replacement: MutationValue::string("ignored").unwrap(),
                },
                GenericMutation::ResourceReplace {
                    target: first_address,
                    path: field.clone(),
                    guard,
                    payload: payload.digest(),
                },
                GenericMutation::ResourceReplace {
                    target: second_address.clone(),
                    path: field.clone(),
                    guard,
                    payload: payload.digest(),
                },
                GenericMutation::ResourceReplace {
                    target: second_address,
                    path: field.clone(),
                    guard,
                    payload: payload.digest(),
                },
            ],
        )
        .unwrap();
        let (_, _, payloads, operations) = plan.into_parts();
        let LocatorResolution::Resolved(first_source) = snapshot
            .state()
            .catalog()
            .classify_locator(operations[0].action().target().source_locator())
        else {
            panic!("first resource source must resolve");
        };
        let mut measured_budget = AssetLoadBudget::default();
        let index = ResourceManifestIndex::build(
            snapshot.state().catalog(),
            operations[0].ordinal(),
            first_source,
            payload.digest(),
            &operations[1..],
            &payloads,
            operations[0].action().target(),
            Some(&field),
            &mut measured_budget,
        )
        .unwrap();
        let first_location =
            resource_sidecar_location(snapshot.state().catalog(), first_source).unwrap();
        let LocatorResolution::Resolved(second_source) = snapshot
            .state()
            .catalog()
            .classify_locator(operations[3].action().target().source_locator())
        else {
            panic!("second resource source must resolve");
        };
        let second_location =
            resource_sidecar_location(snapshot.state().catalog(), second_source).unwrap();

        assert_eq!(index.classified_operations, 3);
        assert_eq!(index.entries.len(), 4);
        assert_eq!(index.domains.len(), 2);
        assert!(index.domains.iter().all(|domain| domain.builder.is_none()));
        assert_eq!(index.domain_entries(first_location).len(), 2);
        assert_eq!(index.domain_entries(second_location).len(), 2);
        let measured = measured_budget.usage();
        drop(index);

        let mut exact_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: measured.entries,
            max_bytes: measured.bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let exact = ResourceManifestIndex::build(
            snapshot.state().catalog(),
            operations[0].ordinal(),
            first_source,
            payload.digest(),
            &operations[1..],
            &payloads,
            operations[0].action().target(),
            Some(&field),
            &mut exact_budget,
        )
        .unwrap();
        assert_eq!(exact_budget.usage(), measured);
        drop(exact);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: measured.entries,
            max_bytes: measured.bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = ResourceManifestIndex::build(
            snapshot.state().catalog(),
            operations[0].ordinal(),
            first_source,
            payload.digest(),
            &operations[1..],
            &payloads,
            operations[0].action().target(),
            Some(&field),
            &mut one_short,
        )
        .unwrap_err();
        assert_eq!(error.0.code, "PREPARE_RESOURCE_INDEX_REJECTED");
        assert!(one_short.usage().bytes < measured.bytes);
    }

    #[test]
    fn external_destination_change_reports_source_and_both_fingerprints() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("main.yaml");
        fs::write(&target, OLD_YAML).unwrap();
        let locator = SourceLocator::path("main.yaml").unwrap();
        let mut workspace = AssetWorkspace::new().unwrap();
        workspace
            .load_source(
                SourceOpenRequest::new(&target, SourceAlias::new("main.yaml").unwrap())
                    .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let snapshot = workspace.snapshot();
        let LocatorResolution::Resolved(source) =
            snapshot.state().catalog().classify_locator(&locator)
        else {
            panic!("loaded destination source must resolve");
        };
        let expected = SourceFingerprint::from_bytes(SourceKind::Yaml, OLD_YAML.as_bytes());
        let actual = SourceFingerprint::from_bytes(SourceKind::Yaml, NEW_YAML.as_bytes());
        let artifacts = one_yaml_artifact("output/0000000000000000");
        let output = artifacts.outputs().next().unwrap();
        let destinations = [PublicationDestination::exact(
            output.name(),
            &target,
            DestinationExpectation::Existing(expected),
        )];
        let proof = DestinationProofSet::observe(
            &artifacts,
            &destinations,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

        fs::write(&target, NEW_YAML).unwrap();
        let conflict = proof
            .revalidate(&mut AssetLoadBudget::default())
            .unwrap_err();
        let destination = OutputDestinationSpec {
            source,
            target,
            expectation: DestinationExpectation::Existing(expected),
        };
        let bindings = [PublicationBinding {
            output: output.name(),
            destination: &destination,
        }];
        let failure = destination_failure(
            conflict,
            "PREPARE_DESTINATION_CHANGED",
            &bindings,
            snapshot.state().catalog(),
            &mut AssetLoadBudget::default(),
        );
        let error = reject(&snapshot, None, failure);
        let diagnostic = &error.report().diagnostics()[0];

        assert_eq!(diagnostic.source(), Some(&locator));
        assert_eq!(diagnostic.expected_fingerprint(), Some(expected));
        assert_eq!(diagnostic.actual_fingerprint(), Some(actual));
    }
}
