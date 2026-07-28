use std::mem::size_of;
use std::sync::Arc;

use unity_asset_core::{
    AssetLoadBudget, BudgetError, Diagnostic, FieldPath, FieldPathSegment, ObjectId,
    RevisionedObjectHandle, SourceId, YamlDocumentSelector,
};

use crate::workspace::WorkspaceView;

use super::cache::{FactCacheCandidate, LocalObjectId, LocalReferenceDiagnostic};
use super::fact::{BinaryExternalReference, RawReferenceTarget, ReferenceFact, ReferenceGuid};
use super::index::{ReferenceIndex, ReferenceIndexInput};
use super::input::{ReferenceInput, ReferenceSource, collect_object_sources};
use super::occurrence::{account_cached_source, scan_source_occurrences};
use super::resolution::{ResolutionCatalog, diagnostic_with_severity};
use super::{
    ReferenceGraphBuildOptions, ReferenceGraphBuildStats, ReferenceGraphCoverage,
    ReferenceGraphError, ReferenceTruncation, ReferenceTruncationKind,
};

struct PendingFact {
    source: RevisionedObjectHandle,
    field_path: FieldPath,
    raw_target: RawReferenceTarget,
    diagnostics: Vec<Diagnostic>,
    invalid: Option<Diagnostic>,
}

struct SourceScanResult<'source> {
    source: ReferenceSource<'source>,
    occurrences: Arc<super::cache::SourceReferenceOccurrences>,
    reused: bool,
}

pub(crate) fn build_graph_from_input<I: ReferenceInput + ?Sized>(
    view: &dyn WorkspaceView,
    reference_input: &I,
    options: ReferenceGraphBuildOptions,
    budget: &mut AssetLoadBudget,
) -> Result<(Arc<ReferenceIndex>, ReferenceGraphBuildStats), ReferenceGraphError> {
    if reference_input.revision() != view.revision()
        || reference_input.workspace_id() != view.workspace_id()
    {
        return Err(ReferenceGraphError::Invariant(
            "WorkspaceView reference input does not match its public context",
        ));
    }
    let store = reference_input.reference_store();
    if let Some(cached) = store.graph(view.revision(), options)? {
        account_cached_graph(&cached, budget)?;
        return Ok((cached, ReferenceGraphBuildStats::new(true, 0)));
    }

    let mut nodes = view.objects(budget)?;
    nodes.sort_unstable();
    if nodes.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(ReferenceGraphError::Invariant(
            "workspace exposed duplicate object handles",
        ));
    }
    let total_nodes = usize_to_u64(nodes.len(), "reference graph node count")?;
    let indexed_nodes = options.max_nodes().unwrap_or(total_nodes).min(total_nodes);
    let indexed_nodes_usize =
        usize::try_from(indexed_nodes).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "reference graph indexed nodes",
        })?;

    let object_source_count = reference_input.object_source_count();
    let scan_inputs = reserve_vec(object_source_count, "reference source scan inputs", budget)?;
    let mut scan_inputs = collect_object_sources(reference_input, scan_inputs)?;
    scan_inputs.sort_unstable_by(|left, right| {
        left.fingerprint()
            .cmp(&right.fingerprint())
            .then_with(|| left.source().cmp(&right.source()))
    });

    let mut scan_results =
        reserve_vec(object_source_count, "reference source scan results", budget)?;
    let mut cache_candidates = reserve_vec(
        object_source_count,
        "reference fact cache candidates",
        budget,
    )?;
    let mut position = 0;
    while position < scan_inputs.len() {
        let fingerprint = scan_inputs[position].fingerprint();
        let end = scan_inputs.partition_point(|source| source.fingerprint() <= fingerprint);
        let group = &scan_inputs[position..end];
        let first = group.first().ok_or(ReferenceGraphError::Invariant(
            "reference source scan group is empty",
        ))?;
        if let Some(hit) = store.fact_hit(fingerprint)? {
            for source in group {
                account_cached_source(&hit, budget)?;
                scan_results.push(SourceScanResult {
                    source: *source,
                    occurrences: Arc::clone(&hit),
                    reused: true,
                });
            }
            cache_candidates.push(FactCacheCandidate {
                fingerprint,
                owner: first.owner(),
                occurrences: hit,
            });
        } else {
            let candidate =
                scan_source_occurrences(first, reference_input.typetree_options(), budget)?;
            cache_candidates.push(FactCacheCandidate {
                fingerprint,
                owner: first.owner(),
                occurrences: Arc::clone(&candidate),
            });
            for (ordinal, source) in group.iter().enumerate() {
                if ordinal != 0 {
                    account_cached_source(&candidate, budget)?;
                }
                scan_results.push(SourceScanResult {
                    source: *source,
                    occurrences: Arc::clone(&candidate),
                    reused: ordinal != 0,
                });
            }
        }
        position = end;
    }
    store.publish_facts_batch(cache_candidates, budget)?;
    scan_results.sort_unstable_by_key(|result| result.source.source());

    let occurrence_capacity = scan_results.iter().try_fold(0_usize, |total, result| {
        total
            .checked_add(result.occurrences.occurrences.len())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "reference pending fact count",
            })
    })?;
    let mut pending = reserve_vec(occurrence_capacity, "reference pending facts", budget)?;
    let mut source_occurrences = reserve_vec(
        object_source_count,
        "reference source occurrence owners",
        budget,
    )?;
    let mut diagnostics = Vec::new();
    let mut source_complete = true;
    let mut reused_source_occurrences = 0_u64;
    for result in scan_results {
        let source_id = result.source.source();
        if result.reused {
            reused_source_occurrences = reused_source_occurrences.checked_add(1).ok_or(
                BudgetError::ArithmeticOverflow {
                    resource: "reused reference source occurrences",
                },
            )?;
        }
        source_complete &= result.occurrences.complete;
        for local in result.occurrences.diagnostics.iter() {
            if let Some(diagnostic) =
                bind_local_diagnostic(reference_input, source_id, local, None, None, budget)?
            {
                push_value(
                    &mut diagnostics,
                    diagnostic,
                    "reference graph diagnostics",
                    budget,
                )?;
            }
        }
        for occurrence in result.occurrences.occurrences.iter() {
            let object = local_object_id(source_id, &occurrence.source, budget)?;
            let Ok(node_ordinal) =
                nodes.binary_search_by(|candidate| candidate.object().cmp(&object))
            else {
                return Err(ReferenceGraphError::Invariant(
                    "format occurrence owner is absent from the workspace object table",
                ));
            };
            if node_ordinal >= indexed_nodes_usize {
                continue;
            }
            let source = clone_handle(
                nodes
                    .get(node_ordinal)
                    .ok_or(ReferenceGraphError::Invariant(
                        "reference owner ordinal is out of bounds",
                    ))?,
                budget,
            )?;
            let field_path =
                clone_field_path(&occurrence.field_path, "reference fact field path", budget)?;
            let raw_target = clone_raw_target(&occurrence.raw_target, budget)?;
            let mut fact_diagnostics = reserve_vec(
                occurrence.diagnostics.len(),
                "reference fact diagnostics",
                budget,
            )?;
            for local in occurrence.diagnostics.iter() {
                if let Some(diagnostic) = bind_local_diagnostic(
                    reference_input,
                    source_id,
                    local,
                    Some(source.object()),
                    Some(&field_path),
                    budget,
                )? {
                    fact_diagnostics.push(diagnostic);
                }
            }
            let invalid = occurrence
                .invalid
                .as_ref()
                .map(|local| {
                    bind_local_diagnostic(
                        reference_input,
                        source_id,
                        local,
                        Some(source.object()),
                        Some(&field_path),
                        budget,
                    )
                })
                .transpose()?
                .flatten();
            pending.push(PendingFact {
                source,
                field_path,
                raw_target,
                diagnostics: fact_diagnostics,
                invalid,
            });
        }
        source_occurrences.push(result.occurrences);
    }
    pending.sort_unstable_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.field_path.cmp(&right.field_path))
            .then_with(|| left.raw_target.cmp(&right.raw_target))
    });
    pending.dedup_by(|left, right| {
        left.source == right.source
            && left.field_path == right.field_path
            && left.raw_target == right.raw_target
    });

    let observed_facts = usize_to_u64(pending.len(), "reference graph observed facts")?;
    let mut facts = {
        // Resolution must see every loaded object. Otherwise a loaded target excluded by the soft
        // node limit would be misclassified as missing.
        let resolver = ResolutionCatalog::build(reference_input, &scan_inputs, &nodes, budget)?;
        let mut facts = reserve_vec(pending.len(), "resolved reference facts", budget)?;
        for pending in pending {
            let resolution = resolver.resolve(
                &pending.source,
                &pending.field_path,
                &pending.raw_target,
                pending.invalid,
                budget,
            )?;
            if let Some(target) = resolution.resolved() {
                let target_ordinal = nodes
                    .binary_search_by(|candidate| candidate.object().cmp(target.object()))
                    .map_err(|_| {
                        ReferenceGraphError::Invariant(
                            "resolved target is absent from the complete workspace node table",
                        )
                    })?;
                if target_ordinal >= indexed_nodes_usize {
                    continue;
                }
            }
            facts.push(ReferenceFact::new(
                pending.source,
                pending.field_path,
                pending.raw_target,
                resolution,
                pending.diagnostics.into_boxed_slice(),
            ));
        }
        facts
    };
    let representable_facts = usize_to_u64(facts.len(), "reference graph representable facts")?;
    let indexed_facts = options
        .max_facts()
        .unwrap_or(representable_facts)
        .min(representable_facts);
    let indexed_facts_usize =
        usize::try_from(indexed_facts).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "reference graph indexed facts",
        })?;
    facts.truncate(indexed_facts_usize);
    nodes.truncate(indexed_nodes_usize);

    let mut truncations = Vec::new();
    if indexed_nodes < total_nodes {
        push_value(
            &mut truncations,
            ReferenceTruncation::new(ReferenceTruncationKind::Nodes, indexed_nodes, total_nodes),
            "reference graph truncations",
            budget,
        )?;
    }
    if indexed_facts < observed_facts {
        push_value(
            &mut truncations,
            ReferenceTruncation::new(
                ReferenceTruncationKind::Facts,
                indexed_facts,
                observed_facts,
            ),
            "reference graph truncations",
            budget,
        )?;
    }
    let complete = source_complete && truncations.is_empty();
    let coverage = ReferenceGraphCoverage::new(
        usize_to_u64(object_source_count, "reference graph source count")?,
        usize_to_u64(source_occurrences.len(), "reference scanned source count")?,
        total_nodes,
        indexed_nodes,
        usize_to_u64(facts.len(), "reference graph fact count")?,
        complete,
        truncations,
    );
    let mut addresses = reserve_vec(nodes.len(), "reference graph object addresses", budget)?;
    for node in &nodes {
        addresses.push(reference_input.address_for_object(node.object(), budget)?);
    }
    let index_input = ReferenceIndexInput {
        workspace: view.workspace_id(),
        revision: view.revision(),
        nodes,
        addresses,
        facts,
        diagnostics,
        coverage,
        source_occurrences,
    };
    let index = ReferenceIndex::build(index_input, budget)?;
    budget.consume_bytes(usize_to_u64(
        size_of::<ReferenceIndex>(),
        "reference graph allocation",
    )?)?;
    let index = store.publish_graph(view.revision(), options, Arc::new(index), budget)?;
    Ok((
        index,
        ReferenceGraphBuildStats::new(false, reused_source_occurrences),
    ))
}

fn local_object_id(
    source: SourceId,
    local: &LocalObjectId,
    budget: &mut AssetLoadBudget,
) -> Result<ObjectId, ReferenceGraphError> {
    match local {
        LocalObjectId::Binary(path_id) => Ok(ObjectId::binary(source, *path_id)?),
        LocalObjectId::Yaml(YamlDocumentSelector::Anchored { anchor }) => Ok(ObjectId::yaml(
            source,
            clone_string(anchor.as_str(), "YAML reference owner anchor", budget)?,
        )?),
        LocalObjectId::Yaml(YamlDocumentSelector::Unanchored { document_index }) => {
            Ok(ObjectId::yaml_document(source, *document_index)?)
        }
    }
}

fn bind_local_diagnostic<I: ReferenceInput + ?Sized>(
    input: &I,
    source: SourceId,
    local: &LocalReferenceDiagnostic,
    default_object: Option<&ObjectId>,
    default_path: Option<&FieldPath>,
    budget: &mut AssetLoadBudget,
) -> Result<Option<Diagnostic>, ReferenceGraphError> {
    let owned_object = local
        .source
        .as_ref()
        .map(|local| local_object_id(source, local, budget))
        .transpose()?;
    let object = owned_object.as_ref().or(default_object);
    let field_path = local.field_path.as_ref().or(default_path);
    let message = clone_string(&local.message, "reference diagnostic message", budget)?;
    let diagnostic = diagnostic_with_severity(
        input,
        object,
        field_path,
        local.severity,
        local.code,
        message,
        budget,
    )?;
    Ok(Some(diagnostic))
}

fn clone_handle(
    handle: &RevisionedObjectHandle,
    budget: &mut AssetLoadBudget,
) -> Result<RevisionedObjectHandle, ReferenceGraphError> {
    budget.consume_bytes(usize_to_u64(
        handle.retained_clone_bytes(),
        "reference fact owner handle",
    )?)?;
    Ok(handle.clone())
}

fn clone_field_path(
    path: &FieldPath,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<FieldPath, ReferenceGraphError> {
    let mut segments = reserve_vec(path.segments().len(), resource, budget)?;
    for segment in path.segments() {
        segments.push(match segment {
            FieldPathSegment::Field(name) => {
                FieldPathSegment::field(clone_string(name, resource, budget)?)?
            }
            FieldPathSegment::Index(index) => FieldPathSegment::Index(*index),
        });
    }
    Ok(FieldPath::from_segments(segments)?)
}

fn clone_raw_target(
    target: &RawReferenceTarget,
    budget: &mut AssetLoadBudget,
) -> Result<RawReferenceTarget, ReferenceGraphError> {
    Ok(match target {
        RawReferenceTarget::Binary {
            file_id,
            path_id,
            external,
        } => RawReferenceTarget::Binary {
            file_id: *file_id,
            path_id: *path_id,
            external: external
                .as_ref()
                .map(|external| {
                    Ok::<_, ReferenceGraphError>(BinaryExternalReference::new(
                        external.index(),
                        external.guid().unwrap_or([0; 16]),
                        external.type_id(),
                        clone_string(external.path(), "reference external path", budget)?,
                    ))
                })
                .transpose()?,
        },
        RawReferenceTarget::Yaml {
            file_id,
            guid,
            type_id,
        } => RawReferenceTarget::Yaml {
            file_id: *file_id,
            guid: guid
                .as_ref()
                .map(|guid| clone_guid(guid, budget))
                .transpose()?,
            type_id: *type_id,
        },
    })
}

fn clone_guid(
    guid: &ReferenceGuid,
    budget: &mut AssetLoadBudget,
) -> Result<ReferenceGuid, ReferenceGraphError> {
    Ok(match guid {
        ReferenceGuid::Parsed(guid) => ReferenceGuid::Parsed(*guid),
        ReferenceGuid::Invalid(value) => ReferenceGuid::Invalid(clone_string(
            value,
            "invalid reference GUID spelling",
            budget,
        )?),
    })
}

fn account_cached_graph(
    graph: &ReferenceIndex,
    budget: &mut AssetLoadBudget,
) -> Result<(), ReferenceGraphError> {
    let nodes = usize_to_u64(graph.nodes().len(), "cached reference graph nodes")?;
    let facts = usize_to_u64(graph.facts().len(), "cached reference graph facts")?;
    budget.consume_entries(
        nodes
            .checked_add(facts)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "cached reference graph entries",
            })?,
    )?;
    budget.consume_members(facts)?;
    Ok(())
}

fn clone_string(
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ReferenceGraphError> {
    let bytes = usize_to_u64(value.len(), resource)?;
    budget.check_bytes(bytes)?;
    let mut cloned = String::new();
    cloned
        .try_reserve_exact(value.len())
        .map_err(|error| ReferenceGraphError::Allocation {
            resource,
            requested: value.len(),
            unit: super::ReferenceAllocationUnit::Bytes,
            source: error,
        })?;
    cloned.push_str(value);
    budget.consume_bytes(bytes)?;
    Ok(cloned)
}

fn reserve_vec<T>(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, ReferenceGraphError> {
    let bytes = capacity
        .checked_mul(size_of::<T>())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(usize_to_u64(bytes, resource)?)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|error| ReferenceGraphError::Allocation {
            resource,
            requested: capacity,
            unit: super::ReferenceAllocationUnit::Elements,
            source: error,
        })?;
    budget.consume_bytes(usize_to_u64(bytes, resource)?)?;
    Ok(values)
}

fn push_value<T>(
    values: &mut Vec<T>,
    value: T,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), ReferenceGraphError> {
    if values.len() == values.capacity() {
        let bytes = usize_to_u64(size_of::<T>(), resource)?;
        budget.check_bytes(bytes)?;
        values
            .try_reserve_exact(1)
            .map_err(|error| ReferenceGraphError::Allocation {
                resource,
                requested: 1,
                unit: super::ReferenceAllocationUnit::Elements,
                source: error,
            })?;
        budget.consume_bytes(bytes)?;
    }
    values.push(value);
    Ok(())
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, BudgetError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use unity_asset_binary::typetree::TypeTreeParseOptions;
    use unity_asset_core::{ObjectAddress, WorkspaceId, WorkspaceRevision};

    use super::*;
    use crate::reference::ReferenceStore;
    use crate::reference::input::sealed;
    use crate::workspace::{AssetWorkspace, ReferenceViewParts, reference_view_parts};

    struct ChangingReferenceInput<'state> {
        inner: ReferenceViewParts<'state>,
        enumerations: Cell<u32>,
    }

    impl sealed::Sealed for ChangingReferenceInput<'_> {}

    impl ReferenceInput for ChangingReferenceInput<'_> {
        fn workspace_id(&self) -> WorkspaceId {
            ReferenceInput::workspace_id(&self.inner)
        }

        fn revision(&self) -> WorkspaceRevision {
            ReferenceInput::revision(&self.inner)
        }

        fn object_source_count(&self) -> usize {
            ReferenceInput::object_source_count(&self.inner)
        }

        fn object_sources(
            &self,
        ) -> impl Iterator<Item = Result<ReferenceSource<'_>, ReferenceGraphError>> {
            let enumeration = self.enumerations.get();
            self.enumerations.set(enumeration.saturating_add(1));
            let sources = if enumeration == 0 {
                ReferenceInput::object_sources(&self.inner).collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            sources.into_iter()
        }

        fn reference_store(&self) -> &ReferenceStore {
            ReferenceInput::reference_store(&self.inner)
        }

        fn typetree_options(&self) -> TypeTreeParseOptions {
            ReferenceInput::typetree_options(&self.inner)
        }

        fn address_for_object(
            &self,
            object: &ObjectId,
            budget: &mut AssetLoadBudget,
        ) -> Result<ObjectAddress, ReferenceGraphError> {
            ReferenceInput::address_for_object(&self.inner, object, budget)
        }
    }

    #[test]
    fn graph_build_freezes_the_reference_source_set_before_resolution() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.prefab");
        fs::write(
            &path,
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Source\n",
        )
        .unwrap();
        let mut workspace = AssetWorkspace::new().unwrap();
        workspace
            .load_path(&path, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        let input = ChangingReferenceInput {
            inner: reference_view_parts(&snapshot),
            enumerations: Cell::new(0),
        };

        build_graph_from_input(
            &snapshot,
            &input,
            ReferenceGraphBuildOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

        assert_eq!(input.enumerations.get(), 1);
    }

    #[test]
    fn graph_build_keeps_its_hit_across_concurrent_rebind_and_sweep() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.prefab");
        let second_path = directory.path().join("second.prefab");
        fs::write(
            &first_path,
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: First\n",
        )
        .unwrap();
        fs::write(
            &second_path,
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &2\nGameObject:\n  m_Name: Second\n",
        )
        .unwrap();

        let mut workspace = AssetWorkspace::new().unwrap();
        workspace
            .load_path(&first_path, &mut AssetLoadBudget::default())
            .unwrap();
        let warm_snapshot = workspace.snapshot();
        let parts = reference_view_parts(&warm_snapshot);
        let first = ReferenceInput::object_sources(&parts)
            .next()
            .unwrap()
            .unwrap();
        let fingerprint = first.fingerprint();
        let store = Arc::clone(parts.store);
        let warm_graph = warm_snapshot
            .reference_graph(
                ReferenceGraphBuildOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(warm_graph.build_stats().source_occurrence_cache_hits(), 0);
        let retained = store.fact_hit(fingerprint).unwrap().unwrap();
        drop(warm_graph);
        drop(warm_snapshot);

        workspace
            .load_path(&second_path, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        let (observed_tx, observed_rx) = mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = mpsc::sync_channel(0);
        store.install_fact_publish_pause(observed_tx, resume_rx);

        let build_snapshot = snapshot.clone();
        let build = thread::spawn(move || {
            build_snapshot.reference_graph(
                ReferenceGraphBuildOptions::default(),
                &mut AssetLoadBudget::default(),
            )
        });
        observed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("build A did not pause before publishing its fact-cache batch");

        let competing_owner = Arc::<[u8]>::from(&b"competing reference owner"[..]);
        store
            .publish_facts_batch(
                vec![FactCacheCandidate {
                    fingerprint,
                    owner: (&competing_owner).into(),
                    occurrences: Arc::clone(&retained),
                }],
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        drop(competing_owner);
        resume_tx.send(()).unwrap();

        let graph = build.join().unwrap().unwrap();
        assert!(graph.is_complete());
        assert_eq!(graph.coverage().scanned_sources(), 2);
        assert_eq!(graph.build_stats().source_occurrence_cache_hits(), 1);
        assert!(Arc::ptr_eq(
            &store.fact_hit(fingerprint).unwrap().unwrap(),
            &retained
        ));
    }
}
