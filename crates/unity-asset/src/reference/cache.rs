use std::mem::size_of;
use std::sync::{Arc, Mutex, Weak};

#[cfg(test)]
use std::sync::mpsc::{Receiver, SyncSender};

use unity_asset_core::{
    AssetLoadBudget, BudgetError, DiagnosticSeverity, FieldPath, SourceFingerprint,
    WorkspaceRevision, YamlDocumentSelector, arc_value_allocation_bytes,
};

use super::fact::RawReferenceTarget;
use super::index::ReferenceIndex;
use super::input::{ReferenceSourceOwner, WeakReferenceSourceOwner};
use super::{ReferenceGraphBuildOptions, ReferenceGraphError};

const REFERENCE_SCANNER_VERSION: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LocalObjectId {
    Binary(i64),
    Yaml(YamlDocumentSelector),
}

#[derive(Debug, Clone)]
pub(crate) struct LocalReferenceDiagnostic {
    pub(crate) severity: DiagnosticSeverity,
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) source: Option<LocalObjectId>,
    pub(crate) field_path: Option<FieldPath>,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalReferenceOccurrence {
    pub(crate) source: LocalObjectId,
    pub(crate) field_path: FieldPath,
    pub(crate) raw_target: RawReferenceTarget,
    pub(crate) diagnostics: Box<[LocalReferenceDiagnostic]>,
    pub(crate) invalid: Option<LocalReferenceDiagnostic>,
}

#[derive(Debug)]
pub(crate) struct SourceReferenceOccurrences {
    pub(crate) occurrences: Box<[LocalReferenceOccurrence]>,
    pub(crate) diagnostics: Box<[LocalReferenceDiagnostic]>,
    pub(crate) object_count: u64,
    pub(crate) complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FactCacheKey {
    fingerprint: SourceFingerprint,
    scanner_version: u8,
}

impl FactCacheKey {
    const fn new(fingerprint: SourceFingerprint) -> Self {
        Self {
            fingerprint,
            scanner_version: REFERENCE_SCANNER_VERSION,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct GraphCacheKey {
    revision: WorkspaceRevision,
    options: ReferenceGraphBuildOptions,
}

#[derive(Debug)]
struct GraphCacheEntry {
    key: GraphCacheKey,
    graph: Weak<ReferenceIndex>,
}

#[derive(Debug)]
struct FactCacheEntry {
    key: FactCacheKey,
    owners: Vec<WeakReferenceSourceOwner>,
    occurrences: Arc<SourceReferenceOccurrences>,
}

impl FactCacheEntry {
    fn is_live(&self) -> bool {
        self.owners.iter().any(WeakReferenceSourceOwner::is_live)
    }
}

pub(crate) struct FactCacheCandidate<'owner> {
    pub(crate) fingerprint: SourceFingerprint,
    pub(crate) owner: ReferenceSourceOwner<'owner>,
    pub(crate) occurrences: Arc<SourceReferenceOccurrences>,
}

#[derive(Debug, Default)]
struct ReferenceStoreState {
    // Source-state owners keep facts alive exactly while matching content remains retained.
    facts: Vec<FactCacheEntry>,
    // A cache lookup must not keep an obsolete revision graph alive.
    graphs: Vec<GraphCacheEntry>,
}

impl ReferenceStoreState {
    fn prepare_graph_insert(
        &mut self,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), ReferenceGraphError> {
        let scanned = self.preflight_sweep(budget)?;
        let live_graphs = self
            .graphs
            .iter()
            .filter(|entry| entry.graph.strong_count() != 0)
            .count();
        if live_graphs == self.graphs.capacity() {
            reserve_cache_slot::<GraphCacheEntry>(
                &mut self.graphs,
                "reference graph cache",
                budget,
            )?;
        }
        self.commit_sweep(scanned, budget)
    }

    fn preflight_sweep(&self, budget: &AssetLoadBudget) -> Result<u64, ReferenceGraphError> {
        let scanned = self
            .facts
            .iter()
            .try_fold(self.graphs.len(), |count, entry| {
                count.checked_add(entry.owners.len())
            })
            .and_then(|count| u64::try_from(count).ok())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "reference cache sweep",
            })?;
        budget.check_entries(scanned)?;
        budget.check_members(scanned)?;
        Ok(scanned)
    }

    fn commit_sweep(
        &mut self,
        scanned: u64,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), ReferenceGraphError> {
        budget.consume_entries(scanned)?;
        budget.consume_members(scanned)?;
        self.facts.retain(FactCacheEntry::is_live);
        self.graphs.retain(|entry| entry.graph.strong_count() != 0);
        Ok(())
    }
}

/// Workspace-owned cache store. Cache residency never participates in revision identity.
#[derive(Debug, Default)]
pub(crate) struct ReferenceStore {
    state: Mutex<ReferenceStoreState>,
    // Candidate stores read through the committed store but publish only to their local state.
    // The committed store never points back to a candidate, so this ownership cannot cycle.
    base: Option<Arc<ReferenceStore>>,
    #[cfg(test)]
    fact_publish_pause: Mutex<Option<FactPublishPause>>,
}

#[cfg(test)]
#[derive(Debug)]
struct FactPublishPause {
    observed: SyncSender<()>,
    resume: Receiver<()>,
}

impl ReferenceStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Creates an isolated candidate cache with read-through access to committed entries.
    pub(crate) fn candidate(
        base: &Arc<Self>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Arc<Self>, ReferenceGraphError> {
        let retained =
            arc_value_allocation_bytes::<Self>().map_err(|_| BudgetError::ArithmeticOverflow {
                resource: "candidate reference store",
            })?;
        budget.check_bytes(retained)?;
        let candidate = Arc::new(Self {
            base: Some(Arc::clone(base)),
            ..Self::default()
        });
        budget.consume_bytes(retained)?;
        Ok(candidate)
    }

    pub(crate) fn fact_hit(
        &self,
        fingerprint: SourceFingerprint,
    ) -> Result<Option<Arc<SourceReferenceOccurrences>>, ReferenceGraphError> {
        let local = {
            let state = self
                .state
                .lock()
                .map_err(|_| ReferenceGraphError::CachePoisoned)?;
            let key = FactCacheKey::new(fingerprint);
            state
                .facts
                .binary_search_by_key(&key, |entry| entry.key)
                .ok()
                .map(|position| Arc::clone(&state.facts[position].occurrences))
        };
        if local.is_some() {
            return Ok(local);
        }
        match &self.base {
            Some(base) => base.fact_hit(fingerprint),
            None => Ok(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn install_fact_publish_pause(
        &self,
        observed: SyncSender<()>,
        resume: Receiver<()>,
    ) {
        *self.fact_publish_pause.lock().unwrap() = Some(FactPublishPause { observed, resume });
    }

    #[cfg(test)]
    fn pause_before_fact_publish(&self) {
        let pause = self.fact_publish_pause.lock().unwrap().take();
        if let Some(pause) = pause {
            pause.observed.send(()).unwrap();
            pause.resume.recv().unwrap();
        }
    }

    pub(crate) fn publish_facts_batch(
        &self,
        candidates: Vec<FactCacheCandidate<'_>>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), ReferenceGraphError> {
        if candidates.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        self.pause_before_fact_publish();
        if candidates.windows(2).any(|pair| {
            FactCacheKey::new(pair[0].fingerprint) >= FactCacheKey::new(pair[1].fingerprint)
        }) {
            return Err(ReferenceGraphError::Invariant(
                "reference fact cache batch is not strictly sorted",
            ));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| ReferenceGraphError::CachePoisoned)?;
        let scanned = state.preflight_sweep(budget)?;
        let plan = plan_fact_merge(&state.facts, &candidates)?;
        let entry_bytes = plan
            .final_count
            .checked_mul(size_of::<FactCacheEntry>())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "reference fact cache batch",
            })?;
        let owner_bytes = plan
            .owner_count
            .checked_mul(size_of::<WeakReferenceSourceOwner>())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "reference fact cache owner cohorts",
            })?;
        let merged_bytes = entry_bytes
            .checked_add(owner_bytes)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "reference fact cache batch",
            })?;

        budget.check_bytes(merged_bytes)?;
        let merged = build_fact_merge(&state.facts, &candidates, &plan)?;
        budget.consume_bytes(merged_bytes)?;
        state.commit_sweep(scanned, budget)?;
        state.facts = merged;
        Ok(())
    }

    pub(crate) fn graph(
        &self,
        revision: WorkspaceRevision,
        options: ReferenceGraphBuildOptions,
    ) -> Result<Option<Arc<ReferenceIndex>>, ReferenceGraphError> {
        let local = {
            let state = self
                .state
                .lock()
                .map_err(|_| ReferenceGraphError::CachePoisoned)?;
            let key = GraphCacheKey { revision, options };
            state
                .graphs
                .binary_search_by_key(&key, |entry| entry.key)
                .ok()
                .and_then(|position| state.graphs[position].graph.upgrade())
        };
        if local.is_some() {
            return Ok(local);
        }
        match &self.base {
            Some(base) => base.graph(revision, options),
            None => Ok(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn local_entry_counts(&self) -> (usize, usize) {
        let state = self.state.lock().expect("reference cache lock");
        (state.facts.len(), state.graphs.len())
    }

    pub(crate) fn publish_graph(
        &self,
        revision: WorkspaceRevision,
        options: ReferenceGraphBuildOptions,
        candidate: Arc<ReferenceIndex>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Arc<ReferenceIndex>, ReferenceGraphError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ReferenceGraphError::CachePoisoned)?;
        let key = GraphCacheKey { revision, options };
        match state.graphs.binary_search_by_key(&key, |entry| entry.key) {
            Ok(position) => {
                let entry = &mut state.graphs[position];
                if let Some(existing) = entry.graph.upgrade() {
                    return Ok(existing);
                }
                *entry = GraphCacheEntry {
                    key,
                    graph: Arc::downgrade(&candidate),
                };
            }
            Err(_) => {
                state.prepare_graph_insert(budget)?;
                let position = state.graphs.partition_point(|existing| existing.key < key);
                state.graphs.insert(
                    position,
                    GraphCacheEntry {
                        key,
                        graph: Arc::downgrade(&candidate),
                    },
                );
            }
        }
        Ok(candidate)
    }
}

struct FactMergePlan {
    final_count: usize,
    owner_count: usize,
    #[cfg(test)]
    key_comparisons: usize,
}

fn plan_fact_merge(
    existing: &[FactCacheEntry],
    candidates: &[FactCacheCandidate<'_>],
) -> Result<FactMergePlan, ReferenceGraphError> {
    let mut final_count = 0_usize;
    let mut owner_count = 0_usize;
    let mut existing_index = 0_usize;
    let mut candidate_index = 0_usize;
    #[cfg(test)]
    let mut key_comparisons = 0_usize;

    while existing_index < existing.len() && candidate_index < candidates.len() {
        #[cfg(test)]
        {
            key_comparisons = key_comparisons.saturating_add(1);
        }
        let candidate_key = FactCacheKey::new(candidates[candidate_index].fingerprint);
        match existing[existing_index].key.cmp(&candidate_key) {
            std::cmp::Ordering::Less => {
                let live_owners = existing[existing_index]
                    .owners
                    .iter()
                    .filter(|owner| owner.is_live())
                    .count();
                if live_owners != 0 {
                    final_count = checked_fact_count(final_count)?;
                    owner_count = checked_owner_count(owner_count, live_owners)?;
                }
                existing_index += 1;
            }
            std::cmp::Ordering::Greater => {
                final_count = checked_fact_count(final_count)?;
                owner_count = checked_owner_count(owner_count, 1)?;
                candidate_index += 1;
            }
            std::cmp::Ordering::Equal => {
                final_count = checked_fact_count(final_count)?;
                let entry = &existing[existing_index];
                let candidate_owner = candidates[candidate_index].owner.downgrade();
                let live_owners = entry.owners.iter().filter(|owner| owner.is_live()).count();
                let candidate_is_retained = entry
                    .owners
                    .iter()
                    .any(|owner| owner.is_live() && owner.ptr_eq(&candidate_owner));
                owner_count = checked_owner_count(
                    owner_count,
                    live_owners + usize::from(!candidate_is_retained),
                )?;
                existing_index += 1;
                candidate_index += 1;
            }
        }
    }
    for entry in &existing[existing_index..] {
        let live_owners = entry.owners.iter().filter(|owner| owner.is_live()).count();
        if live_owners != 0 {
            final_count = checked_fact_count(final_count)?;
            owner_count = checked_owner_count(owner_count, live_owners)?;
        }
    }
    let remaining_candidates = candidates.len().saturating_sub(candidate_index);
    final_count =
        final_count
            .checked_add(remaining_candidates)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "reference fact cache batch",
            })?;
    owner_count = checked_owner_count(owner_count, remaining_candidates)?;

    Ok(FactMergePlan {
        final_count,
        owner_count,
        #[cfg(test)]
        key_comparisons,
    })
}

fn build_fact_merge(
    existing: &[FactCacheEntry],
    candidates: &[FactCacheCandidate<'_>],
    plan: &FactMergePlan,
) -> Result<Vec<FactCacheEntry>, ReferenceGraphError> {
    let mut merged = Vec::new();
    merged
        .try_reserve_exact(plan.final_count)
        .map_err(|source| ReferenceGraphError::Allocation {
            resource: "reference fact cache batch",
            requested: plan.final_count,
            unit: super::ReferenceAllocationUnit::Elements,
            source,
        })?;

    let mut existing_index = 0_usize;
    let mut candidate_index = 0_usize;
    while existing_index < existing.len() && candidate_index < candidates.len() {
        let candidate = &candidates[candidate_index];
        let candidate_key = FactCacheKey::new(candidate.fingerprint);
        match existing[existing_index].key.cmp(&candidate_key) {
            std::cmp::Ordering::Less => {
                if let Some(entry) = merge_fact_entry(Some(&existing[existing_index]), None)? {
                    merged.push(entry);
                }
                existing_index += 1;
            }
            std::cmp::Ordering::Greater => {
                merged.push(
                    merge_fact_entry(None, Some(candidate))?
                        .expect("a candidate always has a live owner"),
                );
                candidate_index += 1;
            }
            std::cmp::Ordering::Equal => {
                merged.push(
                    merge_fact_entry(Some(&existing[existing_index]), Some(candidate))?
                        .expect("a candidate always has a live owner"),
                );
                existing_index += 1;
                candidate_index += 1;
            }
        }
    }
    for entry in &existing[existing_index..] {
        if let Some(entry) = merge_fact_entry(Some(entry), None)? {
            merged.push(entry);
        }
    }
    for candidate in &candidates[candidate_index..] {
        merged.push(
            merge_fact_entry(None, Some(candidate))?.expect("a candidate always has a live owner"),
        );
    }
    debug_assert!(merged.len() <= plan.final_count);
    Ok(merged)
}

fn merge_fact_entry(
    existing: Option<&FactCacheEntry>,
    candidate: Option<&FactCacheCandidate<'_>>,
) -> Result<Option<FactCacheEntry>, ReferenceGraphError> {
    let candidate_owner = candidate.map(|candidate| candidate.owner.downgrade());
    let live_owner_count = existing.map_or(0, |entry| {
        entry.owners.iter().filter(|owner| owner.is_live()).count()
    });
    let candidate_is_retained = candidate_owner.as_ref().is_some_and(|candidate_owner| {
        existing.is_some_and(|entry| {
            entry
                .owners
                .iter()
                .any(|owner| owner.is_live() && owner.ptr_eq(candidate_owner))
        })
    });
    let owner_count =
        live_owner_count + usize::from(candidate_owner.is_some() && !candidate_is_retained);
    if owner_count == 0 {
        return Ok(None);
    }

    let mut owners = Vec::new();
    owners
        .try_reserve_exact(owner_count)
        .map_err(|source| ReferenceGraphError::Allocation {
            resource: "reference fact cache owner cohort",
            requested: owner_count,
            unit: super::ReferenceAllocationUnit::Elements,
            source,
        })?;
    if let Some(existing) = existing {
        owners.extend(
            existing
                .owners
                .iter()
                .filter(|owner| owner.is_live())
                .cloned(),
        );
    }
    if let Some(candidate_owner) = candidate_owner
        && !owners.iter().any(|owner| owner.ptr_eq(&candidate_owner))
    {
        owners.push(candidate_owner);
    }

    let existing_is_live = live_owner_count != 0;
    let key = if let Some(existing) = existing {
        existing.key
    } else {
        FactCacheKey::new(candidate.expect("entry input cannot be empty").fingerprint)
    };
    let occurrences = if existing_is_live {
        Arc::clone(
            &existing
                .expect("a live existing entry must exist")
                .occurrences,
        )
    } else {
        Arc::clone(
            &candidate
                .expect("a dead or absent entry requires a candidate")
                .occurrences,
        )
    };
    Ok(Some(FactCacheEntry {
        key,
        owners,
        occurrences,
    }))
}

fn checked_fact_count(count: usize) -> Result<usize, ReferenceGraphError> {
    count
        .checked_add(1)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "reference fact cache batch",
        })
        .map_err(ReferenceGraphError::from)
}

fn checked_owner_count(count: usize, additional: usize) -> Result<usize, ReferenceGraphError> {
    count
        .checked_add(additional)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "reference fact cache owner cohorts",
        })
        .map_err(ReferenceGraphError::from)
}

fn reserve_cache_slot<T>(
    values: &mut Vec<T>,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), ReferenceGraphError> {
    if values.len() < values.capacity() {
        return Ok(());
    }
    let bytes =
        u64::try_from(size_of::<T>()).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::workspace::{AssetWorkspace, TestSourceBackingOwner, reference_view_parts};
    use unity_asset_core::{AssetLoadLimits, SourceKind};
    use unity_asset_write::artifact::{
        ArtifactBatchDeclaration, ArtifactBudget, ArtifactLimits, PreparedArtifactSet,
    };

    use super::*;
    use crate::reference::occurrence::account_cached_source;

    fn source_owner() -> TestSourceBackingOwner {
        TestSourceBackingOwner::new(SourceKind::Yaml, Arc::from(&b"reference cache owner"[..]))
    }

    fn prepared_owner() -> Arc<PreparedArtifactSet> {
        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut inspection_budget = AssetLoadBudget::default();
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        Arc::new(declaration.seal_output_names().unwrap().finish().unwrap())
    }

    fn fingerprint(ordinal: u64) -> SourceFingerprint {
        SourceFingerprint::from_bytes(SourceKind::Yaml, &ordinal.to_le_bytes())
    }

    fn occurrences(object_count: u64) -> Arc<SourceReferenceOccurrences> {
        Arc::new(SourceReferenceOccurrences {
            occurrences: Box::new([]),
            diagnostics: Box::new([]),
            object_count,
            complete: true,
        })
    }

    #[test]
    fn candidate_cache_reads_committed_facts_without_publishing_its_delta() {
        let committed_owner = source_owner();
        let candidate_owner = source_owner();
        let committed_key = fingerprint(1);
        let candidate_key = fingerprint(2);
        let committed_occurrences = occurrences(1);
        let candidate_occurrences = occurrences(2);
        let committed = Arc::new(ReferenceStore::new());
        committed
            .publish_facts_batch(
                vec![FactCacheCandidate {
                    fingerprint: committed_key,
                    owner: committed_owner.weak().into(),
                    occurrences: Arc::clone(&committed_occurrences),
                }],
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        let candidate =
            ReferenceStore::candidate(&committed, &mut AssetLoadBudget::default()).unwrap();
        let inherited = candidate.fact_hit(committed_key).unwrap().unwrap();
        assert!(Arc::ptr_eq(&inherited, &committed_occurrences));
        candidate
            .publish_facts_batch(
                vec![FactCacheCandidate {
                    fingerprint: candidate_key,
                    owner: candidate_owner.weak().into(),
                    occurrences: Arc::clone(&candidate_occurrences),
                }],
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert!(committed.fact_hit(candidate_key).unwrap().is_none());
        let local = candidate.fact_hit(candidate_key).unwrap().unwrap();
        assert!(Arc::ptr_eq(&local, &candidate_occurrences));
        assert_eq!(committed.local_entry_counts(), (1, 0));
        assert_eq!(candidate.local_entry_counts(), (1, 0));
    }

    #[test]
    fn fact_cache_does_not_retain_committed_or_prepared_owners() {
        let committed = source_owner();
        let prepared = prepared_owner();
        let shared = occurrences(1);
        let store = ReferenceStore::new();
        let mut candidates = vec![
            FactCacheCandidate {
                fingerprint: fingerprint(1),
                owner: committed.weak().into(),
                occurrences: Arc::clone(&shared),
            },
            FactCacheCandidate {
                fingerprint: fingerprint(2),
                owner: (&prepared).into(),
                occurrences: Arc::clone(&shared),
            },
        ];
        candidates.sort_unstable_by_key(|candidate| candidate.fingerprint);

        store
            .publish_facts_batch(candidates, &mut AssetLoadBudget::default())
            .unwrap();

        assert_eq!(committed.strong_count(), 1);
        assert_eq!(Arc::strong_count(&prepared), 1);
        drop(committed);
        drop(prepared);
        assert!(
            store
                .state
                .lock()
                .unwrap()
                .facts
                .iter()
                .all(|entry| !entry.is_live())
        );
    }

    #[test]
    fn fact_cache_retains_every_live_owner_for_the_same_fingerprint() {
        let committed = source_owner();
        let prepared = prepared_owner();
        let retained = occurrences(7);
        let key = fingerprint(10);
        let store = ReferenceStore::new();
        store
            .publish_facts_batch(
                vec![FactCacheCandidate {
                    fingerprint: key,
                    owner: committed.weak().into(),
                    occurrences: Arc::clone(&retained),
                }],
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        let hit = store.fact_hit(key).unwrap().unwrap();
        assert!(Arc::ptr_eq(&hit, &retained));
        store
            .publish_facts_batch(
                vec![FactCacheCandidate {
                    fingerprint: key,
                    owner: (&prepared).into(),
                    occurrences: Arc::clone(&hit),
                }],
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(Arc::strong_count(&prepared), 1);
        {
            let state = store.state.lock().unwrap();
            let entry = &state.facts[state
                .facts
                .binary_search_by_key(&FactCacheKey::new(key), |entry| entry.key)
                .unwrap()];
            assert_eq!(entry.owners.len(), 2);
        }
        drop(prepared);

        let first_sweep_owner = source_owner();
        store
            .publish_facts_batch(
                vec![FactCacheCandidate {
                    fingerprint: fingerprint(20),
                    owner: first_sweep_owner.weak().into(),
                    occurrences: occurrences(20),
                }],
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert!(
            store
                .state
                .lock()
                .unwrap()
                .facts
                .binary_search_by_key(&FactCacheKey::new(key), |entry| entry.key)
                .is_ok()
        );

        drop(committed);
        let second_sweep_owner = source_owner();
        store
            .publish_facts_batch(
                vec![FactCacheCandidate {
                    fingerprint: fingerprint(30),
                    owner: second_sweep_owner.weak().into(),
                    occurrences: occurrences(30),
                }],
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert!(
            store
                .state
                .lock()
                .unwrap()
                .facts
                .binary_search_by_key(&FactCacheKey::new(key), |entry| entry.key)
                .is_err()
        );
    }

    #[test]
    fn failed_cached_accounting_does_not_replace_the_committed_owner() {
        let committed = source_owner();
        let prepared = prepared_owner();
        let retained = occurrences(7);
        let key = fingerprint(15);
        let store = ReferenceStore::new();
        let mut initial = vec![
            FactCacheCandidate {
                fingerprint: key,
                owner: committed.weak().into(),
                occurrences: Arc::clone(&retained),
            },
            FactCacheCandidate {
                fingerprint: fingerprint(16),
                owner: committed.weak().into(),
                occurrences: occurrences(1),
            },
        ];
        initial.sort_unstable_by_key(|candidate| candidate.fingerprint);
        store
            .publish_facts_batch(initial, &mut AssetLoadBudget::default())
            .unwrap();

        let hit = store.fact_hit(key).unwrap().unwrap();
        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 6,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let transition = account_cached_source(&hit, &mut one_short).and_then(|()| {
            store.publish_facts_batch(
                vec![FactCacheCandidate {
                    fingerprint: key,
                    owner: (&prepared).into(),
                    occurrences: Arc::clone(&hit),
                }],
                &mut one_short,
            )
        });
        assert!(matches!(transition, Err(ReferenceGraphError::Budget(_))));
        assert_eq!(one_short.usage(), Default::default());

        let mut no_sweep = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            store.publish_facts_batch(
                vec![FactCacheCandidate {
                    fingerprint: key,
                    owner: (&prepared).into(),
                    occurrences: Arc::clone(&hit),
                }],
                &mut no_sweep,
            ),
            Err(ReferenceGraphError::Budget(_))
        ));
        assert_eq!(no_sweep.usage(), Default::default());
        drop(prepared);

        let repeated = store.fact_hit(key).unwrap().unwrap();
        assert!(Arc::ptr_eq(&repeated, &retained));
        let state = store.state.lock().unwrap();
        let entry = &state.facts[state
            .facts
            .binary_search_by_key(&FactCacheKey::new(key), |entry| entry.key)
            .unwrap()];
        assert_eq!(entry.owners.len(), 1);
        assert!(matches!(
            &entry.owners[0],
            WeakReferenceSourceOwner::Committed(owner) if owner.strong_count() == 1
        ));
    }

    #[test]
    fn cache_churn_is_bounded_by_live_sources_and_graphs() {
        let directory = tempfile::tempdir().unwrap();
        let mut workspace = AssetWorkspace::new().unwrap();

        for index in 0..32_u32 {
            let path = directory.path().join(format!("source-{index}.prefab"));
            fs::write(
                &path,
                format!(
                    "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &{}\nGameObject:\n  m_Name: Source{index}\n",
                    index + 1
                ),
            )
            .unwrap();
            let source = workspace
                .load_path(&path, &mut AssetLoadBudget::default())
                .unwrap();
            let snapshot = workspace.snapshot();
            let graph = snapshot
                .reference_graph(
                    ReferenceGraphBuildOptions::default(),
                    &mut AssetLoadBudget::default(),
                )
                .unwrap();

            {
                let parts = reference_view_parts(&snapshot);
                let cache = parts.store.state.lock().unwrap();
                assert_eq!(cache.facts.len(), 1);
                assert_eq!(cache.graphs.len(), 1);
            }

            drop(graph);
            drop(snapshot);
            workspace
                .unload_source(source, &mut AssetLoadBudget::default())
                .unwrap();
        }

        let snapshot = workspace.snapshot();
        let graph = snapshot
            .reference_graph(
                ReferenceGraphBuildOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let parts = reference_view_parts(&snapshot);
        let cache = parts.store.state.lock().unwrap();
        assert!(cache.facts.is_empty());
        assert_eq!(cache.graphs.len(), 1);
        drop(graph);
    }

    #[test]
    fn failed_fact_cache_growth_does_not_publish_the_candidate() {
        let owner = source_owner();
        let fingerprint = fingerprint(1);
        let store = ReferenceStore::new();
        let candidate = Arc::new(SourceReferenceOccurrences {
            occurrences: Box::new([]),
            diagnostics: Box::new([]),
            object_count: 1,
            complete: true,
        });
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        assert!(matches!(
            store.publish_facts_batch(
                vec![FactCacheCandidate {
                    fingerprint,
                    owner: owner.weak().into(),
                    occurrences: candidate,
                }],
                &mut budget,
            ),
            Err(ReferenceGraphError::Budget(_))
        ));
        assert!(store.state.lock().unwrap().facts.is_empty());
    }

    #[test]
    fn cache_hits_are_logarithmic_and_budget_failure_does_not_start_a_sweep() {
        const CACHED_SOURCES: usize = 32;

        let cached = (0..CACHED_SOURCES)
            .map(|index| {
                (
                    fingerprint(u64::try_from(index).unwrap()),
                    TestSourceBackingOwner::new(
                        SourceKind::Yaml,
                        Arc::<[u8]>::from([u8::try_from(index).unwrap()]),
                    ),
                )
            })
            .collect::<Vec<_>>();
        let next = (
            fingerprint(100),
            TestSourceBackingOwner::new(SourceKind::Yaml, Arc::<[u8]>::from([100])),
        );
        let doomed = (
            fingerprint(101),
            TestSourceBackingOwner::new(SourceKind::Yaml, Arc::<[u8]>::from([101])),
        );

        let store = ReferenceStore::new();
        let candidate = Arc::new(SourceReferenceOccurrences {
            occurrences: Box::new([]),
            diagnostics: Box::new([]),
            object_count: 1,
            complete: true,
        });
        let mut candidates = cached
            .iter()
            .map(|(fingerprint, owner)| FactCacheCandidate {
                fingerprint: *fingerprint,
                owner: owner.weak().into(),
                occurrences: Arc::clone(&candidate),
            })
            .collect::<Vec<_>>();
        candidates.push(FactCacheCandidate {
            fingerprint: doomed.0,
            owner: doomed.1.weak().into(),
            occurrences: Arc::clone(&candidate),
        });
        candidates.sort_unstable_by_key(|candidate| candidate.fingerprint);
        store
            .publish_facts_batch(candidates, &mut AssetLoadBudget::default())
            .unwrap();
        drop(doomed);

        let mut tiny = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let first = &cached[0];
        let hit = store.fact_hit(first.0).unwrap().unwrap();
        assert!(Arc::ptr_eq(&hit, &candidate));

        assert!(matches!(
            store.publish_facts_batch(
                vec![FactCacheCandidate {
                    fingerprint: next.0,
                    owner: next.1.weak().into(),
                    occurrences: Arc::clone(&candidate),
                }],
                &mut tiny,
            ),
            Err(ReferenceGraphError::Budget(_))
        ));
        assert_eq!(tiny.usage().entries, 0);
        assert_eq!(tiny.usage().members, 0);
        let cache = store.state.lock().unwrap();
        assert_eq!(cache.facts.len(), CACHED_SOURCES + 1);
        assert!(cache.facts.iter().any(|entry| !entry.is_live()));
        assert!(
            cache
                .facts
                .binary_search_by_key(&FactCacheKey::new(next.0), |entry| { entry.key })
                .is_err()
        );
    }

    #[test]
    fn fact_batch_sweep_has_exact_budget_boundaries_and_atomic_failure() {
        const EXISTING: u64 = 4;

        let owner = source_owner();
        let candidate = occurrences(1);
        let store = ReferenceStore::new();
        let mut initial = (0..EXISTING)
            .map(|ordinal| FactCacheCandidate {
                fingerprint: fingerprint(ordinal),
                owner: owner.weak().into(),
                occurrences: Arc::clone(&candidate),
            })
            .collect::<Vec<_>>();
        initial.sort_unstable_by_key(|candidate| candidate.fingerprint);
        store
            .publish_facts_batch(initial, &mut AssetLoadBudget::default())
            .unwrap();
        let before = store
            .state
            .lock()
            .unwrap()
            .facts
            .iter()
            .map(|entry| entry.key)
            .collect::<Vec<_>>();

        let make_new = || FactCacheCandidate {
            fingerprint: fingerprint(99),
            owner: owner.weak().into(),
            occurrences: Arc::clone(&candidate),
        };
        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: EXISTING - 1,
            max_members: EXISTING,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            store.publish_facts_batch(vec![make_new()], &mut one_short),
            Err(ReferenceGraphError::Budget(_))
        ));
        assert_eq!(one_short.usage(), Default::default());
        assert_eq!(
            store
                .state
                .lock()
                .unwrap()
                .facts
                .iter()
                .map(|entry| entry.key)
                .collect::<Vec<_>>(),
            before
        );

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: EXISTING,
            max_members: EXISTING,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        store
            .publish_facts_batch(vec![make_new()], &mut exact)
            .unwrap();
        assert_eq!(exact.usage().entries, EXISTING);
        assert_eq!(exact.usage().members, EXISTING);
        assert_eq!(store.state.lock().unwrap().facts.len(), 5);
    }

    #[test]
    fn fact_batch_merge_preserves_live_winners_and_replaces_dead_entries() {
        let owner = source_owner();
        let store = ReferenceStore::new();
        let mut keys = [fingerprint(10), fingerprint(20), fingerprint(30)];
        keys.sort_unstable();
        let old_live = occurrences(1);
        let old_dead = occurrences(2);
        store
            .publish_facts_batch(
                vec![
                    FactCacheCandidate {
                        fingerprint: keys[0],
                        owner: owner.weak().into(),
                        occurrences: Arc::clone(&old_live),
                    },
                    FactCacheCandidate {
                        fingerprint: keys[1],
                        owner: owner.weak().into(),
                        occurrences: Arc::clone(&old_dead),
                    },
                ],
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        {
            let mut state = store.state.lock().unwrap();
            let dead = state
                .facts
                .binary_search_by_key(&FactCacheKey::new(keys[1]), |entry| entry.key)
                .unwrap();
            let temporary = TestSourceBackingOwner::new(SourceKind::Yaml, Arc::<[u8]>::from([]));
            state.facts[dead].owners =
                vec![ReferenceSourceOwner::from(temporary.weak()).downgrade()];
            drop(temporary);
        }

        let replacement_live = occurrences(10);
        let replacement_dead = occurrences(20);
        let inserted = occurrences(30);
        store
            .publish_facts_batch(
                vec![
                    FactCacheCandidate {
                        fingerprint: keys[0],
                        owner: owner.weak().into(),
                        occurrences: replacement_live,
                    },
                    FactCacheCandidate {
                        fingerprint: keys[1],
                        owner: owner.weak().into(),
                        occurrences: Arc::clone(&replacement_dead),
                    },
                    FactCacheCandidate {
                        fingerprint: keys[2],
                        owner: owner.weak().into(),
                        occurrences: Arc::clone(&inserted),
                    },
                ],
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert!(Arc::ptr_eq(
            &store.fact_hit(keys[0]).unwrap().unwrap(),
            &old_live
        ));
        assert!(Arc::ptr_eq(
            &store.fact_hit(keys[1]).unwrap().unwrap(),
            &replacement_dead
        ));
        assert!(Arc::ptr_eq(
            &store.fact_hit(keys[2]).unwrap().unwrap(),
            &inserted
        ));
        let state = store.state.lock().unwrap();
        assert_eq!(state.facts.len(), 3);
        assert!(state.facts.windows(2).all(|pair| pair[0].key < pair[1].key));
    }

    #[test]
    fn fact_batch_housekeeping_remains_linear_above_the_old_failure_threshold() {
        const SOURCE_COUNT: u64 = 2_048;

        let owner = source_owner();
        let shared = occurrences(1);
        let mut keys = (0..SOURCE_COUNT * 2).map(fingerprint).collect::<Vec<_>>();
        keys.sort_unstable();
        let initial = keys
            .iter()
            .step_by(2)
            .map(|fingerprint| FactCacheCandidate {
                fingerprint: *fingerprint,
                owner: owner.weak().into(),
                occurrences: Arc::clone(&shared),
            })
            .collect::<Vec<_>>();
        let additions = keys
            .iter()
            .skip(1)
            .step_by(2)
            .map(|fingerprint| FactCacheCandidate {
                fingerprint: *fingerprint,
                owner: owner.weak().into(),
                occurrences: Arc::clone(&shared),
            })
            .collect::<Vec<_>>();
        assert!(
            initial
                .windows(2)
                .all(|pair| pair[0].fingerprint < pair[1].fingerprint)
        );
        assert!(
            additions
                .windows(2)
                .all(|pair| pair[0].fingerprint < pair[1].fingerprint)
        );

        let store = ReferenceStore::new();
        store
            .publish_facts_batch(initial, &mut AssetLoadBudget::default())
            .unwrap();
        {
            let state = store.state.lock().unwrap();
            let plan = plan_fact_merge(&state.facts, &additions).unwrap();
            assert_eq!(plan.final_count, usize::try_from(SOURCE_COUNT * 2).unwrap());
            assert!(plan.key_comparisons <= usize::try_from(SOURCE_COUNT * 2 - 1).unwrap());
        }
        let mut budget = AssetLoadBudget::default();
        store.publish_facts_batch(additions, &mut budget).unwrap();

        assert_eq!(budget.usage().entries, SOURCE_COUNT);
        assert_eq!(budget.usage().members, SOURCE_COUNT);
        assert_eq!(store.state.lock().unwrap().facts.len(), 4_096);
    }
}
