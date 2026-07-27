use std::{
    collections::{HashMap, TryReserveError, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
};

use thiserror::Error;
use unity_asset_core::{
    DigestV1, FieldPath, FieldPathSegment, ObjectAddress, SourceLocator, WorkspaceId,
    WorkspaceRevision,
};

use super::{
    GenericMutation, MutationPlan, MutationPlanError, MutationPlanFragment, PlanPayload,
    SourceExpectation, validate_operation_count,
};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WriteIndexSummary {
    action_index: usize,
    prefix_len: usize,
    previous_hash_summary: Option<usize>,
}

// Hashes only select allocation-free buckets. Every lookup rechecks the retained action's real
// target and path, so collisions affect lookup work without changing overlap semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WriteSummaryIndex {
    hash_heads: HashMap<u64, usize>,
    summaries: Vec<WriteIndexSummary>,
}

impl WriteSummaryIndex {
    fn reserve(
        &mut self,
        additional: usize,
        resource: &'static str,
    ) -> Result<(), MutationPlanBuilderError> {
        reserve_append(&mut self.summaries, additional, resource)?;
        reserve_index(&mut self.hash_heads, additional, resource)
    }

    fn insert(&mut self, hash: u64, action_index: usize, prefix_len: usize) {
        let summary_index = self.summaries.len();
        let previous_hash_summary = self.hash_heads.insert(hash, summary_index);
        self.summaries.push(WriteIndexSummary {
            action_index,
            prefix_len,
            previous_hash_summary,
        });
    }
}

/// Ordered assembler for one or more independently lowered recipe fragments.
///
/// Generic [`MutationPlan`] construction deliberately permits repeated writes because later
/// operations may consume earlier results. Recipe fragments instead originate from the same base
/// snapshot, so overlapping object or field writes are rejected before assembly.
#[derive(Debug)]
pub struct MutationPlanBuilder {
    workspace_id: WorkspaceId,
    base_revision: WorkspaceRevision,
    sources: Vec<SourceExpectation>,
    payloads: Vec<PlanPayload>,
    actions: Vec<GenericMutation>,
    latest_source_hash_indices: HashMap<u64, usize>,
    previous_source_hash_indices: Vec<Option<usize>>,
    payload_indices: HashMap<DigestV1, usize>,
    target_any_writes: WriteSummaryIndex,
    whole_object_writes: WriteSummaryIndex,
    exact_path_writes: WriteSummaryIndex,
    descendant_prefix_writes: WriteSummaryIndex,
    #[cfg(test)]
    prior_write_index_lookups: usize,
}

impl MutationPlanBuilder {
    /// Creates an assembler bound to one exact workspace identity and base revision.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, base_revision: WorkspaceRevision) -> Self {
        Self {
            workspace_id,
            base_revision,
            sources: Vec::new(),
            payloads: Vec::new(),
            actions: Vec::new(),
            latest_source_hash_indices: HashMap::new(),
            previous_source_hash_indices: Vec::new(),
            payload_indices: HashMap::new(),
            target_any_writes: WriteSummaryIndex::default(),
            whole_object_writes: WriteSummaryIndex::default(),
            exact_path_writes: WriteSummaryIndex::default(),
            descendant_prefix_writes: WriteSummaryIndex::default(),
            #[cfg(test)]
            prior_write_index_lookups: 0,
        }
    }

    /// Returns the workspace identity accepted by this assembler.
    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    /// Returns the base revision accepted by this assembler.
    #[must_use]
    pub const fn base_revision(&self) -> WorkspaceRevision {
        self.base_revision
    }

    pub fn append(
        &mut self,
        fragment: MutationPlanFragment,
    ) -> Result<(), MutationPlanBuilderError> {
        if fragment.workspace_id != self.workspace_id {
            return Err(MutationPlanBuilderError::WorkspaceMismatch {
                expected: self.workspace_id,
                actual: fragment.workspace_id,
            });
        }
        if fragment.base_revision != self.base_revision {
            return Err(MutationPlanBuilderError::RevisionMismatch {
                expected: self.base_revision,
                actual: fragment.base_revision,
            });
        }

        validate_fragment_collections(
            &self.sources,
            &self.latest_source_hash_indices,
            &self.previous_source_hash_indices,
            &fragment.sources,
            &self.payloads,
            &self.payload_indices,
            &fragment.payloads,
        )?;
        let action_base = self.actions.len();
        #[cfg(test)]
        let mut prior_write_index_lookups = 0;
        validate_fragment_writes(
            &self.actions,
            &self.target_any_writes,
            &self.whole_object_writes,
            &self.exact_path_writes,
            &self.descendant_prefix_writes,
            &fragment.actions,
            action_base,
            #[cfg(test)]
            &mut prior_write_index_lookups,
        )?;
        let action_count = self
            .actions
            .len()
            .checked_add(fragment.actions.len())
            .ok_or(MutationPlanError::OperationCountOverflow { count: usize::MAX })?;
        validate_operation_count(action_count)?;
        let prefix_summary_count = fragment
            .actions
            .iter()
            .try_fold(0_usize, |count, action| {
                count.checked_add(mutation_path(action).map_or(0, |path| path.segments().len()))
            })
            .ok_or(MutationPlanBuilderError::IndexEntryCountOverflow {
                resource: "path prefix summaries",
            })?;

        reserve_append(
            &mut self.sources,
            fragment.sources.len(),
            "plan builder sources",
        )?;
        reserve_append(
            &mut self.payloads,
            fragment.payloads.len(),
            "plan builder payloads",
        )?;
        reserve_append(
            &mut self.actions,
            fragment.actions.len(),
            "plan builder actions",
        )?;
        reserve_append(
            &mut self.previous_source_hash_indices,
            fragment.sources.len(),
            "plan builder source hash links",
        )?;
        reserve_index(
            &mut self.latest_source_hash_indices,
            fragment.sources.len(),
            "plan builder source hash index",
        )?;
        reserve_index(
            &mut self.payload_indices,
            fragment.payloads.len(),
            "plan builder payload index",
        )?;
        self.target_any_writes
            .reserve(fragment.actions.len(), "plan builder target-any summaries")?;
        self.whole_object_writes.reserve(
            fragment.actions.len(),
            "plan builder whole-object summaries",
        )?;
        self.exact_path_writes
            .reserve(fragment.actions.len(), "plan builder exact-path summaries")?;
        self.descendant_prefix_writes.reserve(
            prefix_summary_count,
            "plan builder descendant-prefix summaries",
        )?;

        let source_base = self.sources.len();
        self.sources.extend(fragment.sources);
        for index in source_base..self.sources.len() {
            index_committed_source(
                &self.sources,
                index,
                &mut self.latest_source_hash_indices,
                &mut self.previous_source_hash_indices,
            );
        }

        let payload_base = self.payloads.len();
        self.payloads.extend(fragment.payloads);
        for index in payload_base..self.payloads.len() {
            let payload = &self.payloads[index];
            self.payload_indices.entry(payload.digest).or_insert(index);
        }

        self.actions.extend(fragment.actions);
        for index in action_base..self.actions.len() {
            index_committed_action(
                &self.actions,
                index,
                &mut self.target_any_writes,
                &mut self.whole_object_writes,
                &mut self.exact_path_writes,
                &mut self.descendant_prefix_writes,
            );
        }
        #[cfg(test)]
        {
            self.prior_write_index_lookups = self
                .prior_write_index_lookups
                .saturating_add(prior_write_index_lookups);
        }
        Ok(())
    }

    pub fn build(self) -> Result<MutationPlan, MutationPlanError> {
        let Self {
            workspace_id,
            base_revision,
            sources,
            payloads,
            actions,
            ..
        } = self;
        MutationPlan::new(workspace_id, base_revision, sources, payloads, actions)
    }
}

fn validate_fragment_collections(
    existing_sources: &[SourceExpectation],
    latest_source_hash_indices: &HashMap<u64, usize>,
    previous_source_hash_indices: &[Option<usize>],
    appended_sources: &[SourceExpectation],
    existing_payloads: &[PlanPayload],
    payload_indices: &HashMap<DigestV1, usize>,
    appended_payloads: &[PlanPayload],
) -> Result<(), MutationPlanError> {
    for appended in appended_sources {
        if let Some(existing) = find_source_index(
            existing_sources,
            latest_source_hash_indices,
            previous_source_hash_indices,
            &appended.locator,
        )
        .and_then(|index| existing_sources.get(index))
            && existing.fingerprint != appended.fingerprint
        {
            return Err(MutationPlanError::ConflictingSourceExpectation {
                locator: appended.locator.clone(),
                first: existing.fingerprint,
                second: appended.fingerprint,
            });
        }
    }
    for appended in appended_payloads {
        if let Some(existing) = payload_indices
            .get(&appended.digest)
            .and_then(|index| existing_payloads.get(*index))
            && existing.bytes != appended.bytes
        {
            return Err(MutationPlanError::ConflictingPayload(appended.digest));
        }
    }
    Ok(())
}

fn reserve_append<T>(
    values: &mut Vec<T>,
    additional: usize,
    resource: &'static str,
) -> Result<(), MutationPlanBuilderError> {
    values
        .try_reserve(additional)
        .map_err(|error| MutationPlanBuilderError::AllocationFailed {
            resource,
            requested: additional,
            error,
        })
}

fn reserve_index<K: Eq + Hash, V>(
    values: &mut HashMap<K, V>,
    additional: usize,
    resource: &'static str,
) -> Result<(), MutationPlanBuilderError> {
    values
        .try_reserve(additional)
        .map_err(|error| MutationPlanBuilderError::AllocationFailed {
            resource,
            requested: additional,
            error,
        })
}

fn bucket_hash<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn path_bucket_hasher(target_hash: u64) -> DefaultHasher {
    let mut hasher = DefaultHasher::new();
    0x7061_7468_5f76_3100_u64.hash(&mut hasher);
    target_hash.hash(&mut hasher);
    hasher
}

fn next_path_bucket_hash(
    hasher: &mut DefaultHasher,
    segment: &FieldPathSegment,
    prefix_len: usize,
) -> u64 {
    segment.hash(hasher);
    let mut snapshot = hasher.clone();
    prefix_len.hash(&mut snapshot);
    snapshot.finish()
}

fn find_source_index(
    sources: &[SourceExpectation],
    latest_hash_indices: &HashMap<u64, usize>,
    previous_hash_indices: &[Option<usize>],
    locator: &SourceLocator,
) -> Option<usize> {
    let mut cursor = latest_hash_indices.get(&bucket_hash(locator)).copied();
    while let Some(index) = cursor {
        let source = sources.get(index)?;
        if source.locator == *locator {
            return Some(index);
        }
        cursor = previous_hash_indices.get(index).copied().flatten();
    }
    None
}

fn index_committed_source(
    sources: &[SourceExpectation],
    index: usize,
    latest_hash_indices: &mut HashMap<u64, usize>,
    previous_hash_indices: &mut Vec<Option<usize>>,
) {
    let locator = &sources[index].locator;
    if find_source_index(sources, latest_hash_indices, previous_hash_indices, locator).is_some() {
        previous_hash_indices.push(None);
        return;
    }

    let previous = latest_hash_indices.insert(bucket_hash(locator), index);
    previous_hash_indices.push(previous);
}

fn find_target_summary(
    actions: &[GenericMutation],
    index: &WriteSummaryIndex,
    hash: u64,
    target: &ObjectAddress,
) -> Option<usize> {
    let mut cursor = index.hash_heads.get(&hash).copied();
    while let Some(summary_index) = cursor {
        let summary = index.summaries.get(summary_index)?;
        let action = actions.get(summary.action_index)?;
        if action.target() == target {
            return Some(summary.action_index);
        }
        cursor = summary.previous_hash_summary;
    }
    None
}

fn find_exact_path_summary(
    actions: &[GenericMutation],
    index: &WriteSummaryIndex,
    hash: u64,
    target: &ObjectAddress,
    path: &[FieldPathSegment],
) -> Option<usize> {
    let mut cursor = index.hash_heads.get(&hash).copied();
    while let Some(summary_index) = cursor {
        let summary = index.summaries.get(summary_index)?;
        let action = actions.get(summary.action_index)?;
        if summary.prefix_len == path.len()
            && action.target() == target
            && mutation_path(action).is_some_and(|existing| existing.segments() == path)
        {
            return Some(summary.action_index);
        }
        cursor = summary.previous_hash_summary;
    }
    None
}

fn find_descendant_prefix_summary(
    actions: &[GenericMutation],
    index: &WriteSummaryIndex,
    hash: u64,
    target: &ObjectAddress,
    prefix: &[FieldPathSegment],
) -> Option<usize> {
    let mut cursor = index.hash_heads.get(&hash).copied();
    while let Some(summary_index) = cursor {
        let summary = index.summaries.get(summary_index)?;
        let action = actions.get(summary.action_index)?;
        if summary.prefix_len == prefix.len()
            && action.target() == target
            && mutation_path(action).is_some_and(|existing| existing.segments().starts_with(prefix))
        {
            return Some(summary.action_index);
        }
        cursor = summary.previous_hash_summary;
    }
    None
}

fn earlier_index(current: Option<usize>, candidate: Option<usize>) -> Option<usize> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.min(candidate)),
        (Some(current), None) => Some(current),
        (None, candidate) => candidate,
    }
}

#[cfg(test)]
fn record_index_lookup(lookups: &mut usize) {
    *lookups = lookups.saturating_add(1);
}

fn validate_fragment_writes(
    existing: &[GenericMutation],
    target_any_writes: &WriteSummaryIndex,
    whole_object_writes: &WriteSummaryIndex,
    exact_path_writes: &WriteSummaryIndex,
    descendant_prefix_writes: &WriteSummaryIndex,
    appended: &[GenericMutation],
    appended_base: usize,
    #[cfg(test)] prior_write_index_lookups: &mut usize,
) -> Result<(), MutationPlanBuilderError> {
    for (right_offset, right) in appended.iter().enumerate() {
        let target = right.target();
        let target_hash = bucket_hash(target);
        let first_overlapping_index = if let Some(path) = mutation_path(right) {
            #[cfg(test)]
            record_index_lookup(prior_write_index_lookups);
            let mut earliest =
                find_target_summary(existing, whole_object_writes, target_hash, target);
            let segments = path.segments();
            let mut prefix_hasher = path_bucket_hasher(target_hash);
            for (offset, segment) in segments.iter().enumerate() {
                let prefix_len = offset + 1;
                let prefix = &segments[..prefix_len];
                let prefix_hash = next_path_bucket_hash(&mut prefix_hasher, segment, prefix_len);
                #[cfg(test)]
                record_index_lookup(prior_write_index_lookups);
                earliest = if prefix_len == segments.len() {
                    earlier_index(
                        earliest,
                        find_descendant_prefix_summary(
                            existing,
                            descendant_prefix_writes,
                            prefix_hash,
                            target,
                            prefix,
                        ),
                    )
                } else {
                    earlier_index(
                        earliest,
                        find_exact_path_summary(
                            existing,
                            exact_path_writes,
                            prefix_hash,
                            target,
                            prefix,
                        ),
                    )
                };
            }
            earliest
        } else {
            #[cfg(test)]
            record_index_lookup(prior_write_index_lookups);
            find_target_summary(existing, target_any_writes, target_hash, target)
        };

        if let Some(left_index) = first_overlapping_index {
            ensure_non_overlapping(
                &existing[left_index],
                left_index,
                right,
                appended_base + right_offset,
            )?;
        }
        for (left_offset, left) in appended[..right_offset].iter().enumerate() {
            ensure_non_overlapping(
                left,
                appended_base + left_offset,
                right,
                appended_base + right_offset,
            )?;
        }
    }
    Ok(())
}

fn index_committed_action(
    actions: &[GenericMutation],
    action_index: usize,
    target_any_writes: &mut WriteSummaryIndex,
    whole_object_writes: &mut WriteSummaryIndex,
    exact_path_writes: &mut WriteSummaryIndex,
    descendant_prefix_writes: &mut WriteSummaryIndex,
) {
    let action = &actions[action_index];
    let target = action.target();
    let target_hash = bucket_hash(target);
    if find_target_summary(actions, target_any_writes, target_hash, target).is_none() {
        target_any_writes.insert(target_hash, action_index, 0);
    }

    let Some(path) = mutation_path(action) else {
        if find_target_summary(actions, whole_object_writes, target_hash, target).is_none() {
            whole_object_writes.insert(target_hash, action_index, 0);
        }
        return;
    };

    let segments = path.segments();
    let mut prefix_hasher = path_bucket_hasher(target_hash);
    let mut exact_hash = target_hash;
    for (offset, segment) in segments.iter().enumerate() {
        let prefix_len = offset + 1;
        let prefix = &segments[..prefix_len];
        let prefix_hash = next_path_bucket_hash(&mut prefix_hasher, segment, prefix_len);
        if find_descendant_prefix_summary(
            actions,
            descendant_prefix_writes,
            prefix_hash,
            target,
            prefix,
        )
        .is_none()
        {
            descendant_prefix_writes.insert(prefix_hash, action_index, prefix_len);
        }
        exact_hash = prefix_hash;
    }
    if find_exact_path_summary(actions, exact_path_writes, exact_hash, target, segments).is_none() {
        exact_path_writes.insert(exact_hash, action_index, segments.len());
    }
}

fn ensure_non_overlapping(
    left: &GenericMutation,
    left_index: usize,
    right: &GenericMutation,
    right_index: usize,
) -> Result<(), MutationPlanBuilderError> {
    if left.target() != right.target() {
        return Ok(());
    }
    let left_path = mutation_path(left);
    let right_path = mutation_path(right);
    if paths_overlap(left_path, right_path) {
        return Err(MutationPlanBuilderError::OverlappingWrites {
            target: Box::new(left.target().clone()),
            first_index: left_index,
            first_path: Box::new(left_path.cloned()),
            second_index: right_index,
            second_path: Box::new(right_path.cloned()),
        });
    }
    Ok(())
}

fn mutation_path(action: &GenericMutation) -> Option<&FieldPath> {
    match action {
        GenericMutation::FieldReplace { path, .. }
        | GenericMutation::ReferenceReplace { path, .. }
        | GenericMutation::ResourceReplace { path, .. }
        | GenericMutation::SequenceEdit { path, .. } => Some(path),
        GenericMutation::SchemaReplace { .. } | GenericMutation::UnsafeRawReplace { .. } => None,
    }
}

fn paths_overlap(left: Option<&FieldPath>, right: Option<&FieldPath>) -> bool {
    match (left, right) {
        (None, _) | (_, None) => true,
        (Some(left), Some(right)) => {
            left.segments().starts_with(right.segments())
                || right.segments().starts_with(left.segments())
        }
    }
}

/// Failure while assembling independently observed recipe fragments.
#[derive(Debug, Error)]
pub enum MutationPlanBuilderError {
    #[error("recipe fragment targets workspace {actual}, but the builder targets {expected}")]
    WorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("recipe fragment targets revision {actual}, but the builder targets {expected}")]
    RevisionMismatch {
        expected: WorkspaceRevision,
        actual: WorkspaceRevision,
    },
    #[error(
        "recipe writes {first_path:?} at index {first_index} and {second_path:?} at index {second_index} overlap on {target:?}"
    )]
    OverlappingWrites {
        target: Box<ObjectAddress>,
        first_index: usize,
        first_path: Box<Option<FieldPath>>,
        second_index: usize,
        second_path: Box<Option<FieldPath>>,
    },
    #[error("recipe fragment requires too many {resource} index entries")]
    IndexEntryCountOverflow { resource: &'static str },
    #[error("failed to allocate {resource} capacity for {requested} elements: {error}")]
    AllocationFailed {
        resource: &'static str,
        requested: usize,
        #[source]
        error: TryReserveError,
    },
    #[error(transparent)]
    Plan(#[from] MutationPlanError),
}
