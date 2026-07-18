use std::mem::size_of;
use std::ops::Range;

use unity_asset_core::{
    AssetLoadBudget, BudgetError, Diagnostic, ObjectAddress, ObjectId, RevisionedObjectHandle,
    WorkspaceId, WorkspaceRevision,
};

use super::cache::SourceReferenceOccurrences;
use super::fact::ReferenceFact;
use super::{ReferenceGraphCoverage, ReferenceGraphError};

type ObjectRanges = Vec<(ObjectId, Range<usize>)>;
type IncomingIndex = (ObjectRanges, Vec<usize>);
type AdjacencyIndex = (ObjectRanges, Vec<ObjectId>);

#[derive(Debug)]
pub(crate) struct ReferenceIndexInput {
    pub(crate) workspace: WorkspaceId,
    pub(crate) revision: WorkspaceRevision,
    pub(crate) nodes: Vec<RevisionedObjectHandle>,
    pub(crate) addresses: Vec<ObjectAddress>,
    pub(crate) facts: Vec<ReferenceFact>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) coverage: ReferenceGraphCoverage,
    pub(crate) source_occurrences: Vec<std::sync::Arc<SourceReferenceOccurrences>>,
}

#[derive(Debug)]
pub(crate) struct ReferenceIndex {
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    nodes: Box<[RevisionedObjectHandle]>,
    addresses: Box<[ObjectAddress]>,
    facts: Box<[ReferenceFact]>,
    outgoing: Box<[(ObjectId, Range<usize>)]>,
    incoming: Box<[(ObjectId, Range<usize>)]>,
    incoming_ordinals: Box<[usize]>,
    adjacency: Box<[(ObjectId, Range<usize>)]>,
    adjacency_targets: Box<[ObjectId]>,
    reverse_adjacency: Box<[(ObjectId, Range<usize>)]>,
    reverse_targets: Box<[ObjectId]>,
    roots: Box<[usize]>,
    leaves: Box<[usize]>,
    diagnostics: Box<[Diagnostic]>,
    coverage: ReferenceGraphCoverage,
    _source_occurrences: Box<[std::sync::Arc<SourceReferenceOccurrences>]>,
}

impl ReferenceIndex {
    pub(crate) fn build(
        input: ReferenceIndexInput,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ReferenceGraphError> {
        let ReferenceIndexInput {
            workspace,
            revision,
            nodes,
            addresses,
            facts,
            diagnostics,
            coverage,
            source_occurrences,
        } = input;
        if addresses.len() != nodes.len() {
            return Err(ReferenceGraphError::Invariant(
                "reference graph node and address counts differ",
            ));
        }
        let outgoing = build_outgoing(&facts, budget)?;
        let (incoming, incoming_ordinals) = build_incoming(&facts, budget)?;
        let (adjacency, adjacency_targets) = build_adjacency(&facts, false, budget)?;
        let (reverse_adjacency, reverse_targets) = build_adjacency(&facts, true, budget)?;

        let mut roots = reserve_vec(nodes.len(), "reference graph roots", budget)?;
        let mut leaves = reserve_vec(nodes.len(), "reference graph leaves", budget)?;
        for (ordinal, handle) in nodes.iter().enumerate() {
            if lookup_range(&reverse_adjacency, handle.object()).is_none() {
                roots.push(ordinal);
            }
            if lookup_range(&adjacency, handle.object()).is_none() {
                leaves.push(ordinal);
            }
        }

        Ok(Self {
            workspace,
            revision,
            nodes: nodes.into_boxed_slice(),
            addresses: addresses.into_boxed_slice(),
            facts: facts.into_boxed_slice(),
            outgoing: outgoing.into_boxed_slice(),
            incoming: incoming.into_boxed_slice(),
            incoming_ordinals: incoming_ordinals.into_boxed_slice(),
            adjacency: adjacency.into_boxed_slice(),
            adjacency_targets: adjacency_targets.into_boxed_slice(),
            reverse_adjacency: reverse_adjacency.into_boxed_slice(),
            reverse_targets: reverse_targets.into_boxed_slice(),
            roots: roots.into_boxed_slice(),
            leaves: leaves.into_boxed_slice(),
            diagnostics: diagnostics.into_boxed_slice(),
            coverage,
            _source_occurrences: source_occurrences.into_boxed_slice(),
        })
    }

    pub(crate) const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    pub(crate) const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    pub(crate) fn nodes(&self) -> &[RevisionedObjectHandle] {
        &self.nodes
    }

    pub(crate) fn addresses(&self) -> &[ObjectAddress] {
        &self.addresses
    }

    pub(crate) fn facts(&self) -> &[ReferenceFact] {
        &self.facts
    }

    pub(crate) fn outgoing(&self, object: &ObjectId) -> Range<usize> {
        lookup_range(&self.outgoing, object).unwrap_or(0..0)
    }

    pub(crate) fn incoming(&self, object: &ObjectId) -> &[usize] {
        lookup_range(&self.incoming, object)
            .and_then(|range| self.incoming_ordinals.get(range))
            .unwrap_or(&[])
    }

    pub(crate) fn adjacency(&self, object: &ObjectId, reverse: bool) -> &[ObjectId] {
        let (index, targets) = if reverse {
            (&self.reverse_adjacency, &self.reverse_targets)
        } else {
            (&self.adjacency, &self.adjacency_targets)
        };
        lookup_range(index, object)
            .and_then(|range| targets.get(range))
            .unwrap_or(&[])
    }

    pub(crate) fn handle(&self, object: &ObjectId) -> Option<&RevisionedObjectHandle> {
        self.node_ordinal(object)
            .and_then(|index| self.nodes.get(index))
    }

    pub(crate) fn address(&self, object: &ObjectId) -> Option<&ObjectAddress> {
        self.node_ordinal(object)
            .and_then(|index| self.addresses.get(index))
    }

    pub(crate) fn node_ordinal(&self, object: &ObjectId) -> Option<usize> {
        self.nodes
            .binary_search_by(|handle| handle.object().cmp(object))
            .ok()
    }

    pub(crate) fn root_ordinals(&self) -> &[usize] {
        &self.roots
    }

    pub(crate) fn leaf_ordinals(&self) -> &[usize] {
        &self.leaves
    }

    pub(crate) fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub(crate) const fn coverage(&self) -> &ReferenceGraphCoverage {
        &self.coverage
    }
}

fn build_outgoing(
    facts: &[ReferenceFact],
    budget: &mut AssetLoadBudget,
) -> Result<ObjectRanges, ReferenceGraphError> {
    let mut outgoing = reserve_vec(facts.len(), "reference outgoing ranges", budget)?;
    let mut start = 0;
    while start < facts.len() {
        let source = clone_object(
            facts[start].source().object(),
            "reference outgoing identity",
            budget,
        )?;
        let mut end = start + 1;
        while end < facts.len() && facts[end].source().object() == &source {
            end += 1;
        }
        outgoing.push((source, start..end));
        start = end;
    }
    Ok(outgoing)
}

fn build_incoming(
    facts: &[ReferenceFact],
    budget: &mut AssetLoadBudget,
) -> Result<IncomingIndex, ReferenceGraphError> {
    let resolved_count = facts
        .iter()
        .filter(|fact| fact.resolution().resolved().is_some())
        .count();
    let mut pairs = reserve_vec(resolved_count, "reference incoming sort", budget)?;
    for (ordinal, fact) in facts.iter().enumerate() {
        if let Some(target) = fact.resolution().resolved() {
            pairs.push((
                clone_object(target.object(), "reference incoming identity", budget)?,
                ordinal,
            ));
        }
    }
    pairs.sort_unstable();

    let mut ranges = reserve_vec(pairs.len(), "reference incoming ranges", budget)?;
    let mut ordinals = reserve_vec(pairs.len(), "reference incoming ordinals", budget)?;
    let mut position = 0;
    while position < pairs.len() {
        let target = clone_object(
            &pairs[position].0,
            "reference incoming range identity",
            budget,
        )?;
        let start = ordinals.len();
        while position < pairs.len() && pairs[position].0 == target {
            ordinals.push(pairs[position].1);
            position += 1;
        }
        ranges.push((target, start..ordinals.len()));
    }
    Ok((ranges, ordinals))
}

fn build_adjacency(
    facts: &[ReferenceFact],
    reverse: bool,
    budget: &mut AssetLoadBudget,
) -> Result<AdjacencyIndex, ReferenceGraphError> {
    let resolved_count = facts
        .iter()
        .filter(|fact| fact.resolution().resolved().is_some())
        .count();
    let mut edges = reserve_vec(resolved_count, "reference adjacency sort", budget)?;
    for fact in facts {
        let Some(target) = fact.resolution().resolved() else {
            continue;
        };
        let (source, target) = if reverse {
            (target.object(), fact.source().object())
        } else {
            (fact.source().object(), target.object())
        };
        edges.push((
            clone_object(source, "reference adjacency source", budget)?,
            clone_object(target, "reference adjacency target", budget)?,
        ));
    }
    edges.sort_unstable();
    edges.dedup();

    let mut ranges = reserve_vec(edges.len(), "reference adjacency ranges", budget)?;
    let mut targets = reserve_vec(edges.len(), "reference adjacency targets", budget)?;
    let mut position = 0;
    while position < edges.len() {
        let source = clone_object(
            &edges[position].0,
            "reference adjacency range identity",
            budget,
        )?;
        let start = targets.len();
        while position < edges.len() && edges[position].0 == source {
            targets.push(clone_object(
                &edges[position].1,
                "reference adjacency retained target",
                budget,
            )?);
            position += 1;
        }
        ranges.push((source, start..targets.len()));
    }
    Ok((ranges, targets))
}

fn lookup_range(index: &[(ObjectId, Range<usize>)], object: &ObjectId) -> Option<Range<usize>> {
    index
        .binary_search_by(|(candidate, _)| candidate.cmp(object))
        .ok()
        .and_then(|position| index.get(position))
        .map(|(_, range)| range.clone())
}

fn clone_object(
    object: &ObjectId,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<ObjectId, ReferenceGraphError> {
    let retained = u64::try_from(object.retained_clone_bytes())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.consume_bytes(retained)?;
    Ok(object.clone())
}

fn reserve_vec<T>(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, ReferenceGraphError> {
    let bytes = capacity
        .checked_mul(size_of::<T>())
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|error| ReferenceGraphError::Allocation {
            resource,
            requested: capacity,
            unit: super::ReferenceAllocationUnit::Elements,
            source: error,
        })?;
    budget.consume_bytes(bytes)?;
    Ok(values)
}
