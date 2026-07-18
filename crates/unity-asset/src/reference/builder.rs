use std::mem::size_of;
use std::sync::Arc;

use unity_asset_core::{
    AssetLoadBudget, BudgetError, Diagnostic, FieldPath, FieldPathSegment, ObjectId,
    RevisionedObjectHandle, SourceFingerprint, SourceId, SourceKind, YamlDocumentSelector,
};

use crate::workspace::{SourceEntry, WorkspaceView, reference_view_parts};

use super::cache::{FactCacheCandidate, LocalObjectId, LocalReferenceDiagnostic};
use super::fact::{BinaryExternalReference, RawReferenceTarget, ReferenceFact, ReferenceGuid};
use super::index::{ReferenceIndex, ReferenceIndexInput};
use super::occurrence::{account_cached_source, scan_source_occurrences};
use super::resolution::{ResolutionCatalog, address_for_object, diagnostic_with_severity};
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

struct SourceScanInput<'entry> {
    source: SourceId,
    fingerprint: SourceFingerprint,
    entry: &'entry Arc<SourceEntry>,
}

struct SourceScanResult<'entry> {
    source: SourceId,
    fingerprint: SourceFingerprint,
    entry: &'entry Arc<SourceEntry>,
    occurrences: Arc<super::cache::SourceReferenceOccurrences>,
    reused: bool,
}

pub(crate) fn build_graph(
    view: &dyn WorkspaceView,
    options: ReferenceGraphBuildOptions,
    budget: &mut AssetLoadBudget,
) -> Result<(Arc<ReferenceIndex>, ReferenceGraphBuildStats), ReferenceGraphError> {
    let parts = reference_view_parts(view);
    if parts.state.revision() != view.revision() || parts.state.workspace() != view.workspace_id() {
        return Err(ReferenceGraphError::Invariant(
            "WorkspaceView reference input does not match its public context",
        ));
    }
    if let Some(cached) = parts.store.graph(view.revision(), options)? {
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

    let object_source_count = parts
        .state
        .store()
        .iter()
        .filter(|(source, _)| {
            matches!(source.kind(), SourceKind::SerializedFile | SourceKind::Yaml)
        })
        .count();
    let mut scan_inputs = reserve_vec(object_source_count, "reference source scan inputs", budget)?;
    for (source, entry) in parts.state.store().iter() {
        if !matches!(source.kind(), SourceKind::SerializedFile | SourceKind::Yaml) {
            continue;
        }
        let fingerprint = parts
            .state
            .catalog()
            .fingerprint(source)
            .map_err(crate::workspace::WorkspaceError::from)?;
        scan_inputs.push(SourceScanInput {
            source,
            fingerprint,
            entry,
        });
    }
    scan_inputs.sort_unstable_by(|left, right| {
        left.fingerprint
            .cmp(&right.fingerprint)
            .then_with(|| left.source.cmp(&right.source))
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
        let fingerprint = scan_inputs[position].fingerprint;
        let end = scan_inputs.partition_point(|input| input.fingerprint <= fingerprint);
        let group = &scan_inputs[position..end];
        let first = group.first().ok_or(ReferenceGraphError::Invariant(
            "reference source scan group is empty",
        ))?;
        if let Some(cached) = parts.store.facts(fingerprint, first.entry)? {
            for input in group {
                account_cached_source(&cached, budget)?;
                scan_results.push(SourceScanResult {
                    source: input.source,
                    fingerprint,
                    entry: input.entry,
                    occurrences: Arc::clone(&cached),
                    reused: true,
                });
            }
        } else {
            let candidate =
                scan_source_occurrences(first.source, first.entry, parts.typetree, budget)?;
            cache_candidates.push(FactCacheCandidate {
                fingerprint,
                owner: Arc::clone(first.entry),
                occurrences: Arc::clone(&candidate),
            });
            for (ordinal, input) in group.iter().enumerate() {
                if ordinal != 0 {
                    account_cached_source(&candidate, budget)?;
                }
                scan_results.push(SourceScanResult {
                    source: input.source,
                    fingerprint,
                    entry: input.entry,
                    occurrences: Arc::clone(&candidate),
                    reused: ordinal != 0,
                });
            }
        }
        position = end;
    }
    parts.store.publish_facts_batch(cache_candidates, budget)?;

    position = 0;
    while position < scan_results.len() {
        let fingerprint = scan_results[position].fingerprint;
        let end = scan_results.partition_point(|result| result.fingerprint <= fingerprint);
        let owner = scan_results
            .get(end.saturating_sub(1))
            .ok_or(ReferenceGraphError::Invariant(
                "reference source scan result group is empty",
            ))?
            .entry;
        let canonical =
            parts
                .store
                .facts(fingerprint, owner)?
                .ok_or(ReferenceGraphError::Invariant(
                    "published reference facts are absent from the cache",
                ))?;
        for result in &mut scan_results[position..end] {
            result.reused |= !Arc::ptr_eq(&result.occurrences, &canonical);
            result.occurrences = Arc::clone(&canonical);
        }
        position = end;
    }
    scan_results.sort_unstable_by_key(|result| result.source);

    let mut source_occurrences = reserve_vec(
        object_source_count,
        "reference source occurrence owners",
        budget,
    )?;
    let mut diagnostics = Vec::new();
    let mut source_complete = true;
    let mut reused_source_occurrences = 0_u64;
    for result in scan_results {
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
                bind_local_diagnostic(parts.state, result.source, local, None, None, budget)?
            {
                push_value(
                    &mut diagnostics,
                    diagnostic,
                    "reference graph diagnostics",
                    budget,
                )?;
            }
        }
        source_occurrences.push(result.occurrences);
    }

    let occurrence_capacity = source_occurrences
        .iter()
        .try_fold(0_usize, |total, source| {
            total
                .checked_add(source.occurrences.len())
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "reference pending fact count",
                })
        })?;
    let mut pending = reserve_vec(occurrence_capacity, "reference pending facts", budget)?;
    let mut source_cursor = 0_usize;
    for (source_id, _) in parts.state.store().iter() {
        if !matches!(
            source_id.kind(),
            SourceKind::SerializedFile | SourceKind::Yaml
        ) {
            continue;
        }
        let source_facts =
            source_occurrences
                .get(source_cursor)
                .ok_or(ReferenceGraphError::Invariant(
                    "source occurrence cache entry was lost during graph assembly",
                ))?;
        source_cursor += 1;
        for occurrence in source_facts.occurrences.iter() {
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
                    parts.state,
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
                        parts.state,
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
        let resolver = ResolutionCatalog::build(parts.state, &nodes, budget)?;
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
        addresses.push(address_for_object(parts.state, node.object(), budget)?);
    }
    let input = ReferenceIndexInput {
        workspace: view.workspace_id(),
        revision: view.revision(),
        nodes,
        addresses,
        facts,
        diagnostics,
        coverage,
        source_occurrences,
    };
    let index = ReferenceIndex::build(input, budget)?;
    budget.consume_bytes(usize_to_u64(
        size_of::<ReferenceIndex>(),
        "reference graph allocation",
    )?)?;
    let index = parts
        .store
        .publish_graph(view.revision(), options, Arc::new(index), budget)?;
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

fn bind_local_diagnostic(
    state: &crate::workspace::WorkspaceState,
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
        state,
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
