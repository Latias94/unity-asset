use std::iter::FusedIterator;
use std::mem::size_of;
use std::ops::Range;
use std::slice;
use std::sync::Arc;

use unity_asset_core::{
    AssetLoadBudget, BudgetError, ObjectId, RevisionedObjectHandle, WorkspaceId, WorkspaceRevision,
};

use super::fact::ReferenceFact;
use super::index::ReferenceIndex;
use super::{ReferenceGraphCoverage, ReferenceGraphError};

const EMPTY_ORDINAL: usize = usize::MAX;
const INITIAL_COLLECTION_CAPACITY: usize = 8;

/// Direction in which a reference traversal follows resolved edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReferenceDirection {
    Outgoing,
    Incoming,
}

impl ReferenceDirection {
    const fn is_reverse(self) -> bool {
        matches!(self, Self::Incoming)
    }
}

/// Deterministic soft limits for reference traversal work and retained results.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReferenceTraversalLimits {
    max_depth: Option<u32>,
    max_nodes: Option<u64>,
    max_edges: Option<u64>,
    max_components: Option<u64>,
}

impl ReferenceTraversalLimits {
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            max_depth: None,
            max_nodes: None,
            max_edges: None,
            max_components: None,
        }
    }

    #[must_use]
    pub const fn with_max_depth(mut self, maximum: u32) -> Self {
        self.max_depth = Some(maximum);
        self
    }

    #[must_use]
    pub const fn with_max_nodes(mut self, maximum: u64) -> Self {
        self.max_nodes = Some(maximum);
        self
    }

    #[must_use]
    pub const fn with_max_edges(mut self, maximum: u64) -> Self {
        self.max_edges = Some(maximum);
        self
    }

    #[must_use]
    pub const fn with_max_components(mut self, maximum: u64) -> Self {
        self.max_components = Some(maximum);
        self
    }

    #[must_use]
    pub const fn max_depth(self) -> Option<u32> {
        self.max_depth
    }

    #[must_use]
    pub const fn max_nodes(self) -> Option<u64> {
        self.max_nodes
    }

    #[must_use]
    pub const fn max_edges(self) -> Option<u64> {
        self.max_edges
    }

    #[must_use]
    pub const fn max_components(self) -> Option<u64> {
        self.max_components
    }
}

/// Exact soft limits that made a traversal result incomplete.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReferenceTraversalTruncation {
    depth: Option<u32>,
    nodes: Option<u64>,
    edges: Option<u64>,
    components: Option<u64>,
}

impl ReferenceTraversalTruncation {
    #[must_use]
    pub const fn depth_limit(self) -> Option<u32> {
        self.depth
    }

    #[must_use]
    pub const fn node_limit(self) -> Option<u64> {
        self.nodes
    }

    #[must_use]
    pub const fn edge_limit(self) -> Option<u64> {
        self.edges
    }

    #[must_use]
    pub const fn component_limit(self) -> Option<u64> {
        self.components
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.depth.is_none()
            && self.nodes.is_none()
            && self.edges.is_none()
            && self.components.is_none()
    }

    fn into_option(self) -> Option<Self> {
        (!self.is_empty()).then_some(self)
    }
}

#[derive(Debug, Clone)]
enum FactOrdinals<'index> {
    Contiguous(Range<usize>),
    Indexed(slice::Iter<'index, usize>),
}

impl FactOrdinals<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Contiguous(range) => range.len(),
            Self::Indexed(ordinals) => ordinals.len(),
        }
    }
}

impl Iterator for FactOrdinals<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Contiguous(range) => range.next(),
            Self::Indexed(ordinals) => ordinals.next().copied(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let length = self.len();
        (length, Some(length))
    }
}

impl DoubleEndedIterator for FactOrdinals<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            Self::Contiguous(range) => range.next_back(),
            Self::Indexed(ordinals) => ordinals.next_back().copied(),
        }
    }
}

impl ExactSizeIterator for FactOrdinals<'_> {}
impl FusedIterator for FactOrdinals<'_> {}

/// Borrowed, allocation-free view over reference facts selected by an index query.
#[derive(Debug, Clone)]
pub struct ReferenceFacts<'index> {
    index: &'index ReferenceIndex,
    ordinals: FactOrdinals<'index>,
}

impl ReferenceFacts<'_> {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ordinals.len() == 0
    }
}

impl<'index> Iterator for ReferenceFacts<'index> {
    type Item = &'index ReferenceFact;

    fn next(&mut self) -> Option<Self::Item> {
        self.ordinals
            .next()
            .and_then(|ordinal| self.index.facts().get(ordinal))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let length = self.ordinals.len();
        (length, Some(length))
    }
}

impl DoubleEndedIterator for ReferenceFacts<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.ordinals
            .next_back()
            .and_then(|ordinal| self.index.facts().get(ordinal))
    }
}

impl ExactSizeIterator for ReferenceFacts<'_> {}
impl FusedIterator for ReferenceFacts<'_> {}

/// Borrowed, allocation-free view over graph nodes selected by ordinal.
#[derive(Debug, Clone)]
pub struct ReferenceNodes<'index> {
    index: &'index ReferenceIndex,
    ordinals: slice::Iter<'index, usize>,
}

impl ReferenceNodes<'_> {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ordinals.len() == 0
    }
}

impl<'index> Iterator for ReferenceNodes<'index> {
    type Item = &'index RevisionedObjectHandle;

    fn next(&mut self) -> Option<Self::Item> {
        self.ordinals
            .next()
            .and_then(|ordinal| self.index.nodes().get(*ordinal))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let length = self.ordinals.len();
        (length, Some(length))
    }
}

impl DoubleEndedIterator for ReferenceNodes<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.ordinals
            .next_back()
            .and_then(|ordinal| self.index.nodes().get(*ordinal))
    }
}

impl ExactSizeIterator for ReferenceNodes<'_> {}
impl FusedIterator for ReferenceNodes<'_> {}

/// Stable breadth-first closure over one revision-bound reference index.
#[derive(Debug)]
pub struct ReferenceTraversal {
    index: Arc<ReferenceIndex>,
    direction: ReferenceDirection,
    ordinals: Vec<usize>,
    truncation: Option<ReferenceTraversalTruncation>,
}

impl ReferenceTraversal {
    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        self.index.workspace()
    }

    #[must_use]
    pub fn revision(&self) -> WorkspaceRevision {
        self.index.revision()
    }

    #[must_use]
    pub const fn direction(&self) -> ReferenceDirection {
        self.direction
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.truncation.is_none() && self.index.coverage().is_complete()
    }

    #[must_use]
    pub const fn truncation(&self) -> Option<ReferenceTraversalTruncation> {
        self.truncation
    }

    #[must_use]
    pub fn coverage(&self) -> &ReferenceGraphCoverage {
        self.index.coverage()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.ordinals.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ordinals.is_empty()
    }

    #[must_use]
    pub fn nodes(&self) -> ReferenceNodes<'_> {
        ReferenceNodes {
            index: &self.index,
            ordinals: self.ordinals.iter(),
        }
    }
}

impl<'index> IntoIterator for &'index ReferenceTraversal {
    type Item = &'index RevisionedObjectHandle;
    type IntoIter = ReferenceNodes<'index>;

    fn into_iter(self) -> Self::IntoIter {
        self.nodes()
    }
}

/// Strongly connected components that contain at least one directed cycle.
#[derive(Debug)]
pub struct ReferenceCycles {
    index: Arc<ReferenceIndex>,
    components: Vec<Vec<usize>>,
    truncation: Option<ReferenceTraversalTruncation>,
}

impl ReferenceCycles {
    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        self.index.workspace()
    }

    #[must_use]
    pub fn revision(&self) -> WorkspaceRevision {
        self.index.revision()
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.truncation.is_none() && self.index.coverage().is_complete()
    }

    #[must_use]
    pub const fn truncation(&self) -> Option<ReferenceTraversalTruncation> {
        self.truncation
    }

    #[must_use]
    pub fn coverage(&self) -> &ReferenceGraphCoverage {
        self.index.coverage()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.components.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }

    pub fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = ReferenceNodes<'_>> + ExactSizeIterator + FusedIterator + '_
    {
        self.components.iter().map(|ordinals| ReferenceNodes {
            index: &self.index,
            ordinals: ordinals.iter(),
        })
    }
}

pub(crate) fn outgoing<'index>(
    index: &'index ReferenceIndex,
    source: &RevisionedObjectHandle,
) -> Result<ReferenceFacts<'index>, ReferenceGraphError> {
    validate_indexed_handle(index, source)?;
    Ok(ReferenceFacts {
        index,
        ordinals: FactOrdinals::Contiguous(index.outgoing(source.object())),
    })
}

pub(crate) fn incoming<'index>(
    index: &'index ReferenceIndex,
    target: &RevisionedObjectHandle,
) -> Result<ReferenceFacts<'index>, ReferenceGraphError> {
    validate_indexed_handle(index, target)?;
    Ok(ReferenceFacts {
        index,
        ordinals: FactOrdinals::Indexed(index.incoming(target.object()).iter()),
    })
}

pub(crate) fn roots(index: &ReferenceIndex) -> ReferenceNodes<'_> {
    ReferenceNodes {
        index,
        ordinals: index.root_ordinals().iter(),
    }
}

pub(crate) fn leaves(index: &ReferenceIndex) -> ReferenceNodes<'_> {
    ReferenceNodes {
        index,
        ordinals: index.leaf_ordinals().iter(),
    }
}

pub(crate) fn closure(
    index: &Arc<ReferenceIndex>,
    roots: &[RevisionedObjectHandle],
    direction: ReferenceDirection,
    limits: ReferenceTraversalLimits,
    budget: &mut AssetLoadBudget,
) -> Result<ReferenceTraversal, ReferenceGraphError> {
    validate_traversal_root_contexts(index, roots)?;
    let root_count = u64::try_from(roots.len()).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "reference_traversal_roots",
    })?;
    budget.check_members(root_count)?;
    budget.consume_members(root_count)?;
    validate_indexed_roots(index, roots)?;

    let node_capacity = soft_usize_limit(limits.max_nodes, index.nodes().len());
    let mut truncation = ReferenceTraversalTruncation::default();
    let (mut ordinals, mut visited, roots_truncated) =
        collect_canonical_roots(index, roots, node_capacity, budget)?;
    if roots_truncated {
        truncation.nodes = limits.max_nodes;
    }

    let mut cursor = 0;
    let mut level_end = ordinals.len();
    let mut depth = 0_u32;
    let mut edge_count = 0_u64;

    budget.observe_depth(0)?;
    'breadth_first: while cursor < ordinals.len() {
        if cursor == level_end {
            depth = depth
                .checked_add(1)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "reference_traversal_depth",
                })?;
            level_end = ordinals.len();
        }

        let ordinal = ordinals[cursor];
        cursor += 1;
        let handle = index
            .nodes()
            .get(ordinal)
            .ok_or(ReferenceGraphError::Invariant(
                "reference traversal node ordinal is not indexed",
            ))?;
        let neighbors = index.adjacency(handle.object(), direction.is_reverse());

        for neighbor in neighbors {
            if !admit_closure_edge(limits, &mut edge_count, &mut truncation, budget)? {
                break 'breadth_first;
            }
            let neighbor = object_ordinal(index, neighbor)?;
            if visited.contains(neighbor) {
                continue;
            }
            if limits.max_depth.is_some_and(|maximum| depth >= maximum) {
                truncation.depth = limits.max_depth;
                continue;
            }
            if ordinals.len() >= node_capacity {
                truncation.nodes = limits.max_nodes;
                break 'breadth_first;
            }

            let next_depth = depth
                .checked_add(1)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "reference_traversal_depth",
                })?;
            budget.observe_depth(next_depth)?;
            push_discovered(&mut ordinals, &mut visited, neighbor, node_capacity, budget)?;
        }
    }

    Ok(ReferenceTraversal {
        index: Arc::clone(index),
        direction,
        ordinals,
        truncation: truncation.into_option(),
    })
}

pub(crate) fn cycles(
    index: Arc<ReferenceIndex>,
    limits: ReferenceTraversalLimits,
    budget: &mut AssetLoadBudget,
) -> Result<ReferenceCycles, ReferenceGraphError> {
    let selected_count = soft_usize_limit(limits.max_nodes, index.nodes().len());
    let mut truncation = ReferenceTraversalTruncation::default();
    if selected_count < index.nodes().len() {
        truncation.nodes = limits.max_nodes;
    }

    let selected_count_u64 =
        u64::try_from(selected_count).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "reference_cycle_nodes",
        })?;
    budget.check_entries(selected_count_u64)?;
    budget.consume_entries(selected_count_u64)?;

    let limited_edges =
        build_limited_cycle_edges(&index, selected_count, limits, &mut truncation, budget)?;
    let edges_precharged = limited_edges.is_some();
    let mut depth_limited_outgoing = limits.max_depth.map(|_| Vec::new());
    let mut visited = reserve_filled_vec(selected_count, 0_u8, budget, "reference cycle marks")?;
    let mut finish = reserve_vec(selected_count, budget, "reference cycle finish order")?;
    let mut stack = Vec::<DfsFrame>::new();

    for root in 0..selected_count {
        if visited[root] != 0 {
            continue;
        }
        budget.observe_depth(0)?;
        visited[root] = 1;
        push_frame(&mut stack, DfsFrame::new(root, 0), budget)?;

        while let Some(frame) = stack.last_mut() {
            let Some(neighbor) = cycle_neighbor(
                &index,
                limited_edges.as_ref(),
                frame.node,
                false,
                frame.next_neighbor,
                selected_count,
            )?
            else {
                let completed = stack.pop().ok_or(ReferenceGraphError::Invariant(
                    "reference cycle DFS stack unexpectedly became empty",
                ))?;
                finish.push(completed.node);
                continue;
            };
            frame.next_neighbor += 1;
            if !edges_precharged {
                budget.consume_members(1)?;
            }
            let source = frame.node;
            let next_depth = if visited[neighbor] == 0 {
                let next_depth =
                    frame
                        .depth
                        .checked_add(1)
                        .ok_or(BudgetError::ArithmeticOverflow {
                            resource: "reference_cycle_depth",
                        })?;
                if limits.max_depth.is_some_and(|maximum| next_depth > maximum) {
                    truncation.depth = limits.max_depth;
                    continue;
                }
                Some(next_depth)
            } else {
                None
            };
            if let Some(outgoing) = depth_limited_outgoing.as_mut() {
                push_cycle_projection_edge(outgoing, source, neighbor, budget)?;
            }
            let Some(next_depth) = next_depth else {
                continue;
            };
            budget.observe_depth(next_depth)?;
            visited[neighbor] = 1;
            push_frame(&mut stack, DfsFrame::new(neighbor, next_depth), budget)?;
        }
    }

    let traversal_edges = if let Some(outgoing) = depth_limited_outgoing {
        Some(finalize_cycle_edges(outgoing, budget)?)
    } else {
        limited_edges
    };

    visited.fill(0);
    let mut components = Vec::<Vec<usize>>::new();
    for root in finish.into_iter().rev() {
        if visited[root] != 0 {
            continue;
        }

        let mut component = Vec::<usize>::new();
        let mut has_self_loop = false;
        budget.observe_depth(0)?;
        visited[root] = 1;
        push_component_node(&mut component, root, budget)?;
        push_frame(&mut stack, DfsFrame::new(root, 0), budget)?;

        while let Some(frame) = stack.last_mut() {
            let Some(neighbor) = cycle_neighbor(
                &index,
                traversal_edges.as_ref(),
                frame.node,
                true,
                frame.next_neighbor,
                selected_count,
            )?
            else {
                stack.pop();
                continue;
            };
            frame.next_neighbor += 1;
            has_self_loop |= neighbor == frame.node;
            if visited[neighbor] != 0 {
                continue;
            }

            let next_depth = frame
                .depth
                .checked_add(1)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "reference_cycle_depth",
                })?;
            if limits.max_depth.is_some_and(|maximum| next_depth > maximum) {
                truncation.depth = limits.max_depth;
                return Ok(ReferenceCycles {
                    index,
                    components: Vec::new(),
                    truncation: truncation.into_option(),
                });
            }
            budget.observe_depth(next_depth)?;
            visited[neighbor] = 1;
            push_component_node(&mut component, neighbor, budget)?;
            push_frame(&mut stack, DfsFrame::new(neighbor, next_depth), budget)?;
        }

        if component.len() > 1 || has_self_loop {
            component.sort_unstable();
            let maximum = soft_usize_limit(limits.max_components, usize::MAX);
            if components.len() >= maximum {
                truncation.components = limits.max_components;
                if maximum != 0 && component < components[0] {
                    components[0] = component;
                    sift_down_max_component(&mut components, 0);
                }
            } else {
                reserve_push_capacity(
                    &mut components,
                    maximum,
                    budget,
                    "reference cycle components",
                )?;
                budget.consume_entries(1)?;
                components.push(component);
                if limits.max_components.is_some() {
                    let position = components.len() - 1;
                    sift_up_max_component(&mut components, position);
                }
            }
        }
    }

    components.sort_unstable();
    Ok(ReferenceCycles {
        index,
        components,
        truncation: truncation.into_option(),
    })
}

fn validate_indexed_handle(
    index: &ReferenceIndex,
    handle: &RevisionedObjectHandle,
) -> Result<(), ReferenceGraphError> {
    handle.validate_context(index.workspace(), index.revision())?;
    if index.handle(handle.object()).is_none() {
        return Err(ReferenceGraphError::ObjectNotIndexed);
    }
    Ok(())
}

fn validate_traversal_root_contexts(
    index: &ReferenceIndex,
    roots: &[RevisionedObjectHandle],
) -> Result<(), ReferenceGraphError> {
    for root in roots {
        root.validate_context(index.workspace(), index.revision())?;
    }
    Ok(())
}

fn validate_indexed_roots(
    index: &ReferenceIndex,
    roots: &[RevisionedObjectHandle],
) -> Result<(), ReferenceGraphError> {
    for root in roots {
        if index.handle(root.object()).is_none() {
            return Err(ReferenceGraphError::ObjectNotIndexed);
        }
    }
    Ok(())
}

fn collect_canonical_roots(
    index: &ReferenceIndex,
    roots: &[RevisionedObjectHandle],
    maximum: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(Vec<usize>, OrdinalSet, bool), ReferenceGraphError> {
    let mut ordinals = Vec::new();
    let mut selected = OrdinalSet::new();
    let mut truncated = false;
    let bounded = maximum < index.nodes().len();
    for root in roots {
        let ordinal = object_ordinal(index, root.object())?;
        if selected.contains(ordinal) {
            continue;
        }
        if ordinals.len() < maximum {
            push_discovered(&mut ordinals, &mut selected, ordinal, maximum, budget)?;
            if bounded {
                let position = ordinals.len() - 1;
                sift_up_max_ordinal(&mut ordinals, position);
            }
        } else if maximum != 0 && ordinal < ordinals[0] {
            truncated = true;
            let evicted = ordinals[0];
            if !selected.remove(evicted) {
                return Err(ReferenceGraphError::Invariant(
                    "reference root heap diverged from its membership index",
                ));
            }
            ordinals[0] = ordinal;
            selected.insert_prepared(ordinal);
            sift_down_max_ordinal(&mut ordinals, 0);
        } else {
            truncated = true;
        }
    }
    ordinals.sort_unstable();
    Ok((ordinals, selected, truncated))
}

fn admit_closure_edge(
    limits: ReferenceTraversalLimits,
    count: &mut u64,
    truncation: &mut ReferenceTraversalTruncation,
    budget: &mut AssetLoadBudget,
) -> Result<bool, ReferenceGraphError> {
    if limits.max_edges.is_some_and(|maximum| *count >= maximum) {
        truncation.edges = limits.max_edges;
        return Ok(false);
    }
    budget.consume_members(1)?;
    *count = count
        .checked_add(1)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "reference_traversal_edges",
        })?;
    Ok(true)
}

fn push_discovered(
    ordinals: &mut Vec<usize>,
    visited: &mut OrdinalSet,
    ordinal: usize,
    maximum: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), ReferenceGraphError> {
    budget.check_entries(1)?;
    reserve_push_capacity(ordinals, maximum, budget, "reference traversal queue")?;
    visited.ensure_insert_capacity(budget)?;
    budget.consume_entries(1)?;
    visited.insert_prepared(ordinal);
    ordinals.push(ordinal);
    Ok(())
}

fn object_ordinal(index: &ReferenceIndex, object: &ObjectId) -> Result<usize, ReferenceGraphError> {
    index
        .nodes()
        .binary_search_by(|handle| handle.object().cmp(object))
        .map_err(|_| ReferenceGraphError::Invariant("reference adjacency target is not indexed"))
}

fn soft_usize_limit(limit: Option<u64>, available: usize) -> usize {
    limit
        .and_then(|maximum| usize::try_from(maximum).ok())
        .map_or(available, |maximum| maximum.min(available))
}

#[derive(Debug, Clone, Copy)]
struct DfsFrame {
    node: usize,
    next_neighbor: usize,
    depth: u32,
}

impl DfsFrame {
    const fn new(node: usize, depth: u32) -> Self {
        Self {
            node,
            next_neighbor: 0,
            depth,
        }
    }
}

#[derive(Debug)]
struct LimitedCycleEdges {
    outgoing: Vec<(usize, usize)>,
    incoming: Vec<(usize, usize)>,
}

fn build_limited_cycle_edges(
    index: &ReferenceIndex,
    selected_count: usize,
    limits: ReferenceTraversalLimits,
    truncation: &mut ReferenceTraversalTruncation,
    budget: &mut AssetLoadBudget,
) -> Result<Option<LimitedCycleEdges>, ReferenceGraphError> {
    let Some(maximum) = limits.max_edges else {
        return Ok(None);
    };
    let maximum = usize::try_from(maximum).unwrap_or(usize::MAX);
    let mut outgoing = Vec::<(usize, usize)>::new();

    'sources: for source in 0..selected_count {
        let handle = index
            .nodes()
            .get(source)
            .ok_or(ReferenceGraphError::Invariant(
                "reference cycle source ordinal is not indexed",
            ))?;
        for target in index.adjacency(handle.object(), false) {
            let target = object_ordinal(index, target)?;
            if target >= selected_count {
                break;
            }
            if outgoing.len() >= maximum {
                truncation.edges = limits.max_edges;
                break 'sources;
            }
            budget.consume_members(1)?;
            reserve_push_capacity(
                &mut outgoing,
                maximum,
                budget,
                "reference cycle edge projection",
            )?;
            outgoing.push((source, target));
        }
    }

    Ok(Some(finalize_cycle_edges(outgoing, budget)?))
}

fn push_cycle_projection_edge(
    outgoing: &mut Vec<(usize, usize)>,
    source: usize,
    target: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), ReferenceGraphError> {
    reserve_push_capacity(
        outgoing,
        usize::MAX,
        budget,
        "depth-limited reference cycle edge projection",
    )?;
    outgoing.push((source, target));
    Ok(())
}

fn finalize_cycle_edges(
    mut outgoing: Vec<(usize, usize)>,
    budget: &mut AssetLoadBudget,
) -> Result<LimitedCycleEdges, ReferenceGraphError> {
    outgoing.sort_unstable();
    let mut incoming = reserve_vec(
        outgoing.len(),
        budget,
        "reverse reference cycle edge projection",
    )?;
    incoming.extend(outgoing.iter().map(|(source, target)| (*target, *source)));
    incoming.sort_unstable();
    Ok(LimitedCycleEdges { outgoing, incoming })
}

fn cycle_neighbor(
    index: &ReferenceIndex,
    limited: Option<&LimitedCycleEdges>,
    node: usize,
    reverse: bool,
    position: usize,
    selected_count: usize,
) -> Result<Option<usize>, ReferenceGraphError> {
    if let Some(limited) = limited {
        let edges = if reverse {
            &limited.incoming
        } else {
            &limited.outgoing
        };
        let start = edges.partition_point(|(source, _)| *source < node);
        let end = edges.partition_point(|(source, _)| *source <= node);
        return Ok(edges
            .get(start.saturating_add(position))
            .filter(|_| start.saturating_add(position) < end)
            .map(|(_, target)| *target)
            .filter(|target| *target < selected_count));
    }

    let handle = index
        .nodes()
        .get(node)
        .ok_or(ReferenceGraphError::Invariant(
            "reference cycle node ordinal is not indexed",
        ))?;
    let neighbor = index
        .adjacency(handle.object(), reverse)
        .get(position)
        .map(|object| object_ordinal(index, object))
        .transpose()?;
    // Nodes and adjacency share ObjectId order, so the first omitted prefix ordinal ends the slice.
    Ok(neighbor.filter(|neighbor| *neighbor < selected_count))
}

fn push_frame(
    stack: &mut Vec<DfsFrame>,
    frame: DfsFrame,
    budget: &mut AssetLoadBudget,
) -> Result<(), ReferenceGraphError> {
    reserve_push_capacity(stack, usize::MAX, budget, "reference cycle DFS stack")?;
    stack.push(frame);
    Ok(())
}

fn push_component_node(
    component: &mut Vec<usize>,
    ordinal: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), ReferenceGraphError> {
    budget.check_entries(1)?;
    reserve_push_capacity(component, usize::MAX, budget, "reference cycle component")?;
    budget.consume_entries(1)?;
    component.push(ordinal);
    Ok(())
}

fn sift_up_max_component(components: &mut [Vec<usize>], mut position: usize) {
    while position != 0 {
        let parent = (position - 1) / 2;
        if components[parent] >= components[position] {
            break;
        }
        components.swap(parent, position);
        position = parent;
    }
}

fn sift_down_max_component(components: &mut [Vec<usize>], mut position: usize) {
    loop {
        let Some(left) = position
            .checked_mul(2)
            .and_then(|value| value.checked_add(1))
        else {
            return;
        };
        if left >= components.len() {
            return;
        }
        let right = left + 1;
        let largest = if right < components.len() && components[right] > components[left] {
            right
        } else {
            left
        };
        if components[position] >= components[largest] {
            return;
        }
        components.swap(position, largest);
        position = largest;
    }
}

fn sift_up_max_ordinal(ordinals: &mut [usize], mut position: usize) {
    while position != 0 {
        let parent = (position - 1) / 2;
        if ordinals[parent] >= ordinals[position] {
            break;
        }
        ordinals.swap(parent, position);
        position = parent;
    }
}

fn sift_down_max_ordinal(ordinals: &mut [usize], mut position: usize) {
    loop {
        let Some(left) = position
            .checked_mul(2)
            .and_then(|value| value.checked_add(1))
        else {
            return;
        };
        if left >= ordinals.len() {
            return;
        }
        let right = left + 1;
        let largest = if right < ordinals.len() && ordinals[right] > ordinals[left] {
            right
        } else {
            left
        };
        if ordinals[position] >= ordinals[largest] {
            return;
        }
        ordinals.swap(position, largest);
        position = largest;
    }
}

fn reserve_push_capacity<T>(
    values: &mut Vec<T>,
    maximum: usize,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<(), ReferenceGraphError> {
    if values.len() < values.capacity() {
        return Ok(());
    }
    let doubled = values.capacity().saturating_mul(2);
    let target = doubled
        .max(INITIAL_COLLECTION_CAPACITY.min(maximum))
        .min(maximum);
    if target <= values.capacity() {
        return Err(ReferenceGraphError::Invariant(
            "reference query attempted to exceed a soft collection limit",
        ));
    }
    let added_capacity = target - values.capacity();
    charge_allocation::<T>(added_capacity, budget, resource)?;
    values
        .try_reserve_exact(target - values.len())
        .map_err(|error| ReferenceGraphError::Allocation {
            resource,
            requested: target - values.len(),
            unit: super::ReferenceAllocationUnit::Elements,
            source: error,
        })
}

fn reserve_vec<T>(
    capacity: usize,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<Vec<T>, ReferenceGraphError> {
    charge_allocation::<T>(capacity, budget, resource)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|error| ReferenceGraphError::Allocation {
            resource,
            requested: capacity,
            unit: super::ReferenceAllocationUnit::Elements,
            source: error,
        })?;
    Ok(values)
}

fn reserve_filled_vec<T: Clone>(
    capacity: usize,
    value: T,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<Vec<T>, ReferenceGraphError> {
    let mut values = reserve_vec(capacity, budget, resource)?;
    values.resize(capacity, value);
    Ok(values)
}

fn charge_allocation<T>(
    capacity: usize,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<(), ReferenceGraphError> {
    let bytes = capacity
        .checked_mul(size_of::<T>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

#[derive(Debug, Default)]
struct OrdinalSet {
    slots: Vec<usize>,
    len: usize,
}

impl OrdinalSet {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            len: 0,
        }
    }

    fn contains(&self, ordinal: usize) -> bool {
        if self.slots.is_empty() {
            return false;
        }
        let mask = self.slots.len() - 1;
        let mut slot = ordinal_hash(ordinal) & mask;
        loop {
            match self.slots[slot] {
                EMPTY_ORDINAL => return false,
                current if current == ordinal => return true,
                _ => slot = (slot + 1) & mask,
            }
        }
    }

    fn remove(&mut self, ordinal: usize) -> bool {
        if self.slots.is_empty() {
            return false;
        }
        let mask = self.slots.len() - 1;
        let mut hole = ordinal_hash(ordinal) & mask;
        loop {
            match self.slots[hole] {
                EMPTY_ORDINAL => return false,
                current if current == ordinal => break,
                _ => hole = (hole + 1) & mask,
            }
        }

        let mut scan = (hole + 1) & mask;
        while self.slots[scan] != EMPTY_ORDINAL {
            let home = ordinal_hash(self.slots[scan]) & mask;
            let scan_distance = scan.wrapping_sub(home) & mask;
            let hole_distance = hole.wrapping_sub(home) & mask;
            if scan_distance > hole_distance {
                self.slots[hole] = self.slots[scan];
                hole = scan;
            }
            scan = (scan + 1) & mask;
        }
        self.slots[hole] = EMPTY_ORDINAL;
        self.len -= 1;
        true
    }

    fn ensure_insert_capacity(
        &mut self,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), ReferenceGraphError> {
        if self.len < self.slots.len() / 2 {
            return Ok(());
        }
        let capacity = if self.slots.is_empty() {
            INITIAL_COLLECTION_CAPACITY
        } else {
            self.slots
                .len()
                .checked_mul(2)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "reference traversal visited set",
                })?
        };
        let mut replacement = reserve_filled_vec(
            capacity,
            EMPTY_ORDINAL,
            budget,
            "reference traversal visited set",
        )?;
        for ordinal in self
            .slots
            .iter()
            .copied()
            .filter(|ordinal| *ordinal != EMPTY_ORDINAL)
        {
            insert_ordinal_slot(&mut replacement, ordinal);
        }
        self.slots = replacement;
        Ok(())
    }

    fn insert_prepared(&mut self, ordinal: usize) {
        insert_ordinal_slot(&mut self.slots, ordinal);
        self.len += 1;
    }
}

fn insert_ordinal_slot(slots: &mut [usize], ordinal: usize) {
    let mask = slots.len() - 1;
    let mut slot = ordinal_hash(ordinal) & mask;
    while slots[slot] != EMPTY_ORDINAL {
        slot = (slot + 1) & mask;
    }
    slots[slot] = ordinal;
}

const fn ordinal_hash(ordinal: usize) -> usize {
    ordinal.wrapping_mul(0x9e37_79b9_usize)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use unity_asset_core::{
        AssetLoadBudget, AssetLoadLimits, BudgetError, DigestV1, FieldPath, ObjectAddress,
        ObjectId, SourceId, SourceKind, SourceLocator, WorkspaceId, WorkspaceRevision,
    };

    use super::*;
    use crate::reference::fact::{RawReferenceTarget, ReferenceResolution};
    use crate::reference::index::ReferenceIndexInput;
    use crate::reference::{ReferenceGraphCoverage, ReferenceGraphError};

    struct GraphFixture {
        index: Arc<ReferenceIndex>,
        handles: Vec<RevisionedObjectHandle>,
    }

    fn graph(node_count: usize, edges: &[(usize, usize)]) -> GraphFixture {
        let workspace = WorkspaceId::from_u128(1).expect("workspace identity");
        let revision = WorkspaceRevision::new(DigestV1::hash_bytes(b"reference query tests"));
        let source = SourceId::new(workspace, SourceKind::SerializedFile, 1)
            .expect("serialized source identity");
        let mut handles = (0..node_count)
            .map(|ordinal| {
                let path_id = i64::try_from(ordinal + 1).expect("test pathID");
                let object = ObjectId::binary(source, path_id).expect("binary object identity");
                RevisionedObjectHandle::new(workspace, revision, object)
                    .expect("revisioned object handle")
            })
            .collect::<Vec<_>>();
        handles.sort_by(|left, right| left.object().cmp(right.object()));
        let locator = SourceLocator::path("query.assets").expect("source locator");
        let addresses = handles
            .iter()
            .map(|handle| {
                ObjectAddress::binary_at(
                    locator.clone(),
                    handle.object().binary_path_id().expect("binary pathID"),
                )
                .expect("object address")
            })
            .collect();

        let mut facts = edges
            .iter()
            .enumerate()
            .map(|(edge, (source, target))| {
                let path_id = handles[*target]
                    .object()
                    .binary_path_id()
                    .expect("binary pathID");
                ReferenceFact::new(
                    handles[*source].clone(),
                    FieldPath::root()
                        .push_field(format!("edge_{edge:08}"))
                        .expect("field path"),
                    RawReferenceTarget::Binary {
                        file_id: 0,
                        path_id,
                        external: None,
                    },
                    ReferenceResolution::Resolved(handles[*target].clone()),
                    Vec::new().into_boxed_slice(),
                )
            })
            .collect::<Vec<_>>();
        facts.sort_by(|left, right| {
            left.source()
                .object()
                .cmp(right.source().object())
                .then_with(|| left.field_path().cmp(right.field_path()))
                .then_with(|| left.raw_target().cmp(right.raw_target()))
        });

        let coverage = ReferenceGraphCoverage::new(
            1,
            1,
            u64::try_from(node_count).expect("node count"),
            u64::try_from(node_count).expect("node count"),
            u64::try_from(facts.len()).expect("fact count"),
            true,
            Vec::new(),
        );
        let mut budget = AssetLoadBudget::default();
        let index = ReferenceIndex::build(
            ReferenceIndexInput {
                workspace,
                revision,
                nodes: handles.clone(),
                addresses,
                facts,
                diagnostics: Vec::new(),
                coverage,
                source_occurrences: Vec::new(),
            },
            &mut budget,
        )
        .expect("reference index");
        GraphFixture {
            index: Arc::new(index),
            handles,
        }
    }

    fn path_ids<'index>(
        handles: impl IntoIterator<Item = &'index RevisionedObjectHandle>,
    ) -> Vec<i64> {
        handles
            .into_iter()
            .map(|handle| handle.object().binary_path_id().expect("binary pathID"))
            .collect()
    }

    #[test]
    fn borrowed_queries_preserve_canonical_fact_and_node_order() {
        let fixture = graph(3, &[(0, 1), (0, 2), (2, 1)]);

        let outgoing = outgoing(&fixture.index, &fixture.handles[0]).expect("outgoing facts");
        assert_eq!(outgoing.len(), 2);
        assert_eq!(
            outgoing
                .map(|fact| {
                    fact.resolution()
                        .resolved()
                        .and_then(|target| target.object().binary_path_id())
                        .expect("resolved binary target")
                })
                .collect::<Vec<_>>(),
            vec![2, 3]
        );

        let incoming = incoming(&fixture.index, &fixture.handles[1]).expect("incoming facts");
        assert_eq!(
            incoming
                .map(|fact| fact
                    .source()
                    .object()
                    .binary_path_id()
                    .expect("source pathID"))
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(path_ids(roots(&fixture.index)), vec![1]);
        assert_eq!(path_ids(leaves(&fixture.index)), vec![2]);
    }

    #[test]
    fn closure_is_stable_breadth_first_in_both_directions() {
        let fixture = graph(4, &[(0, 2), (0, 1), (1, 3), (2, 3)]);
        let mut budget = AssetLoadBudget::default();
        let outgoing = closure(
            &fixture.index,
            &[fixture.handles[0].clone()],
            ReferenceDirection::Outgoing,
            ReferenceTraversalLimits::unbounded(),
            &mut budget,
        )
        .expect("outgoing closure");
        assert!(outgoing.is_complete());
        assert_eq!(path_ids(outgoing.nodes()), vec![1, 2, 3, 4]);

        let mut budget = AssetLoadBudget::default();
        let incoming = closure(
            &fixture.index,
            &[fixture.handles[3].clone()],
            ReferenceDirection::Incoming,
            ReferenceTraversalLimits::unbounded(),
            &mut budget,
        )
        .expect("incoming closure");
        assert_eq!(path_ids(incoming.nodes()), vec![4, 2, 3, 1]);
    }

    #[test]
    fn closure_reports_each_soft_limit_without_converting_hard_budget_errors() {
        let fixture = graph(4, &[(0, 1), (0, 2), (1, 3)]);

        let mut budget = AssetLoadBudget::default();
        let depth_limited = closure(
            &fixture.index,
            &[fixture.handles[0].clone()],
            ReferenceDirection::Outgoing,
            ReferenceTraversalLimits::unbounded().with_max_depth(0),
            &mut budget,
        )
        .expect("depth-limited closure");
        assert_eq!(path_ids(depth_limited.nodes()), vec![1]);
        assert_eq!(
            depth_limited
                .truncation()
                .and_then(ReferenceTraversalTruncation::depth_limit),
            Some(0)
        );

        let mut budget = AssetLoadBudget::default();
        let node_limited = closure(
            &fixture.index,
            &[fixture.handles[0].clone()],
            ReferenceDirection::Outgoing,
            ReferenceTraversalLimits::unbounded().with_max_nodes(2),
            &mut budget,
        )
        .expect("node-limited closure");
        assert_eq!(path_ids(node_limited.nodes()), vec![1, 2]);
        assert_eq!(
            node_limited
                .truncation()
                .and_then(ReferenceTraversalTruncation::node_limit),
            Some(2)
        );

        let mut budget = AssetLoadBudget::default();
        let edge_limited = closure(
            &fixture.index,
            &[fixture.handles[0].clone()],
            ReferenceDirection::Outgoing,
            ReferenceTraversalLimits::unbounded().with_max_edges(1),
            &mut budget,
        )
        .expect("edge-limited closure");
        assert_eq!(path_ids(edge_limited.nodes()), vec![1, 2]);
        assert_eq!(
            edge_limited
                .truncation()
                .and_then(ReferenceTraversalTruncation::edge_limit),
            Some(1)
        );

        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            ..AssetLoadLimits::default()
        })
        .expect("hard query budget");
        assert!(matches!(
            closure(
                &fixture.index,
                &[fixture.handles[0].clone()],
                ReferenceDirection::Outgoing,
                ReferenceTraversalLimits::unbounded(),
                &mut budget,
            ),
            Err(ReferenceGraphError::Budget(BudgetError::Exceeded {
                resource: "entries",
                ..
            }))
        ));

        let roots_only = graph(4, &[]);
        let mut budget = AssetLoadBudget::default();
        let canonical_roots = closure(
            &roots_only.index,
            &[
                roots_only.handles[3].clone(),
                roots_only.handles[1].clone(),
                roots_only.handles[2].clone(),
                roots_only.handles[0].clone(),
            ],
            ReferenceDirection::Outgoing,
            ReferenceTraversalLimits::unbounded().with_max_nodes(2),
            &mut budget,
        )
        .expect("root-limited closure");
        assert_eq!(path_ids(canonical_roots.nodes()), vec![1, 2]);
        assert_eq!(
            canonical_roots
                .truncation()
                .and_then(ReferenceTraversalTruncation::node_limit),
            Some(2)
        );
    }

    #[test]
    fn stale_roots_fail_before_query_budget_is_touched() {
        let fixture = graph(2, &[(0, 1)]);
        let stale_revision = WorkspaceRevision::new(DigestV1::hash_bytes(b"stale revision"));
        let unknown = RevisionedObjectHandle::new(
            fixture.index.workspace(),
            fixture.index.revision(),
            ObjectId::binary(fixture.handles[0].object().source(), 999)
                .expect("unknown binary object"),
        )
        .expect("unknown handle");
        let stale = RevisionedObjectHandle::new(
            fixture.index.workspace(),
            stale_revision,
            fixture.handles[0].object().clone(),
        )
        .expect("stale handle");
        let mut budget = AssetLoadBudget::default();
        let before = budget.usage();

        assert!(matches!(
            closure(
                &fixture.index,
                &[unknown.clone(), stale],
                ReferenceDirection::Outgoing,
                ReferenceTraversalLimits::unbounded(),
                &mut budget,
            ),
            Err(ReferenceGraphError::Contract(_))
        ));
        assert_eq!(budget.usage(), before);

        assert!(matches!(
            closure(
                &fixture.index,
                &[unknown],
                ReferenceDirection::Outgoing,
                ReferenceTraversalLimits::unbounded(),
                &mut budget,
            ),
            Err(ReferenceGraphError::ObjectNotIndexed)
        ));
        let after_unknown = budget.usage();
        assert_eq!(after_unknown.members, before.members + 1);
        assert_eq!(after_unknown.entries, before.entries);
        assert_eq!(after_unknown.bytes, before.bytes);
    }

    #[test]
    fn iterative_cycles_are_canonical_and_component_bounded() {
        let fixture = graph(5, &[(0, 1), (1, 0), (2, 2), (3, 4)]);
        let mut budget = AssetLoadBudget::default();
        let result = cycles(
            Arc::clone(&fixture.index),
            ReferenceTraversalLimits::unbounded(),
            &mut budget,
        )
        .expect("cycles");
        assert!(result.is_complete());
        assert_eq!(
            result.iter().map(path_ids).collect::<Vec<_>>(),
            vec![vec![1, 2], vec![3]]
        );

        let mut budget = AssetLoadBudget::default();
        let component_limited = cycles(
            Arc::clone(&fixture.index),
            ReferenceTraversalLimits::unbounded().with_max_components(1),
            &mut budget,
        )
        .expect("component-limited cycles");
        assert_eq!(
            component_limited.iter().map(path_ids).collect::<Vec<_>>(),
            vec![vec![1, 2]]
        );
        assert_eq!(
            component_limited
                .truncation()
                .and_then(ReferenceTraversalTruncation::component_limit),
            Some(1)
        );

        let mut budget = AssetLoadBudget::default();
        let edge_limited = cycles(
            Arc::clone(&fixture.index),
            ReferenceTraversalLimits::unbounded().with_max_edges(1),
            &mut budget,
        )
        .expect("edge-limited cycles");
        assert!(edge_limited.is_empty());
        assert_eq!(
            edge_limited
                .truncation()
                .and_then(ReferenceTraversalTruncation::edge_limit),
            Some(1)
        );

        let mut exact_member_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .expect("exact edge-member budget");
        let exact_member_result = cycles(
            Arc::clone(&fixture.index),
            ReferenceTraversalLimits::unbounded().with_max_edges(1),
            &mut exact_member_budget,
        )
        .expect("soft edge limit precedes hard member budget");
        assert_eq!(
            exact_member_result
                .truncation()
                .and_then(ReferenceTraversalTruncation::edge_limit),
            Some(1)
        );
        assert_eq!(exact_member_budget.usage().members, 1);

        let mut zero_edge_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .expect("zero-edge budget");
        let zero_edges = cycles(
            Arc::clone(&fixture.index),
            ReferenceTraversalLimits::unbounded().with_max_edges(0),
            &mut zero_edge_budget,
        )
        .expect("zero-edge cycle projection");
        assert!(zero_edges.is_empty());
        assert_eq!(zero_edge_budget.usage().members, 0);

        let depth_fixture = graph(3, &[(0, 1), (1, 2), (2, 0)]);
        let mut budget = AssetLoadBudget::default();
        let depth_limited = cycles(
            depth_fixture.index,
            ReferenceTraversalLimits::unbounded().with_max_depth(1),
            &mut budget,
        )
        .expect("depth-limited cycles");
        assert!(depth_limited.is_empty());
        assert_eq!(
            depth_limited
                .truncation()
                .and_then(ReferenceTraversalTruncation::depth_limit),
            Some(1)
        );
    }

    #[test]
    fn cycle_detection_uses_an_explicit_stack_for_deep_components() {
        const NODE_COUNT: usize = 2_048;
        let mut edges = (0..NODE_COUNT - 1)
            .map(|source| (source, source + 1))
            .collect::<Vec<_>>();
        edges.push((NODE_COUNT - 1, 0));
        let fixture = graph(NODE_COUNT, &edges);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: u32::try_from(NODE_COUNT).expect("test depth"),
            ..AssetLoadLimits::default()
        })
        .expect("deep traversal budget");

        let result = cycles(
            fixture.index,
            ReferenceTraversalLimits::unbounded(),
            &mut budget,
        )
        .expect("deep cycle");
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.iter().next().expect("cycle component").len(),
            NODE_COUNT
        );
    }

    #[test]
    fn cycle_node_prefix_does_not_scan_the_omitted_adjacency_suffix() {
        const NODE_COUNT: usize = 4_096;
        let edges = (1..NODE_COUNT)
            .map(|target| (0, target))
            .collect::<Vec<_>>();
        let fixture = graph(NODE_COUNT, &edges);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .expect("tight member budget");

        let result = cycles(
            fixture.index,
            ReferenceTraversalLimits::unbounded().with_max_nodes(1),
            &mut budget,
        )
        .expect("node-prefix cycles");
        assert!(result.is_empty());
        assert_eq!(budget.usage().members, 0);
        assert_eq!(
            result
                .truncation()
                .and_then(ReferenceTraversalTruncation::node_limit),
            Some(1)
        );
    }

    #[test]
    fn many_independent_cycles_are_sorted_without_ordered_insertion() {
        const NODE_COUNT: usize = 4_096;
        let edges = (0..NODE_COUNT).map(|node| (node, node)).collect::<Vec<_>>();
        let fixture = graph(NODE_COUNT, &edges);
        let mut budget = AssetLoadBudget::default();

        let result = cycles(
            fixture.index,
            ReferenceTraversalLimits::unbounded(),
            &mut budget,
        )
        .expect("independent cycles");
        assert_eq!(result.len(), NODE_COUNT);
        let mut components = result.iter();
        assert_eq!(
            path_ids(components.next().expect("first component")),
            vec![1]
        );
        assert_eq!(
            path_ids(components.next_back().expect("last component")),
            vec![i64::try_from(NODE_COUNT).expect("last pathID")]
        );
    }

    #[test]
    fn many_reverse_ordered_roots_are_canonicalized_without_ordered_insertion() {
        const NODE_COUNT: usize = 4_096;
        let fixture = graph(NODE_COUNT, &[]);
        let roots = fixture.handles.iter().rev().cloned().collect::<Vec<_>>();
        let mut budget = AssetLoadBudget::default();

        let result = closure(
            &fixture.index,
            &roots,
            ReferenceDirection::Outgoing,
            ReferenceTraversalLimits::unbounded(),
            &mut budget,
        )
        .expect("reverse-ordered roots");
        assert!(result.is_complete());
        assert_eq!(result.len(), NODE_COUNT);
        let mut nodes = result.nodes();
        assert_eq!(path_ids(nodes.by_ref().take(1)), vec![1]);
        assert_eq!(
            path_ids(nodes.rev().take(1)),
            vec![i64::try_from(NODE_COUNT).expect("last pathID")]
        );
    }
}
