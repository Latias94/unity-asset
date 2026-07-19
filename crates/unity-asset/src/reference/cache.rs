use std::mem::size_of;
use std::sync::{Arc, Mutex, Weak};

use unity_asset_core::{
    AssetLoadBudget, BudgetError, DiagnosticSeverity, FieldPath, SourceFingerprint,
    WorkspaceRevision, YamlDocumentSelector,
};

use crate::workspace::SourceEntry;

use super::fact::RawReferenceTarget;
use super::index::ReferenceIndex;
use super::{ReferenceGraphBuildOptions, ReferenceGraphError};

const REFERENCE_SCANNER_VERSION: u8 = 1;

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
    owner: Weak<[u8]>,
    occurrences: Arc<SourceReferenceOccurrences>,
}

pub(crate) struct FactCacheCandidate {
    pub(crate) fingerprint: SourceFingerprint,
    pub(crate) owner: Arc<SourceEntry>,
    pub(crate) occurrences: Arc<SourceReferenceOccurrences>,
}

#[derive(Debug, Default)]
struct ReferenceStoreState {
    // Content-addressed backings keep facts alive exactly while matching source state is retained.
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
            .len()
            .checked_add(self.graphs.len())
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
        self.facts.retain(|entry| entry.owner.strong_count() != 0);
        self.graphs.retain(|entry| entry.graph.strong_count() != 0);
        Ok(())
    }
}

/// Workspace-owned cache store. Cache residency never participates in revision identity.
#[derive(Debug, Default)]
pub(crate) struct ReferenceStore {
    state: Mutex<ReferenceStoreState>,
}

impl ReferenceStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn facts(
        &self,
        fingerprint: SourceFingerprint,
        owner: &Arc<SourceEntry>,
    ) -> Result<Option<Arc<SourceReferenceOccurrences>>, ReferenceGraphError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ReferenceGraphError::CachePoisoned)?;
        let key = FactCacheKey::new(fingerprint);
        let Ok(position) = state.facts.binary_search_by_key(&key, |entry| entry.key) else {
            return Ok(None);
        };
        state.facts[position].owner = Arc::downgrade(owner.image().backing());
        Ok(Some(Arc::clone(&state.facts[position].occurrences)))
    }

    pub(crate) fn publish_facts_batch(
        &self,
        candidates: Vec<FactCacheCandidate>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), ReferenceGraphError> {
        if candidates.is_empty() {
            return Ok(());
        }
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
        let final_count = plan_fact_merge(&state.facts, &candidates)?.final_count;
        let merged_bytes = final_count
            .checked_mul(size_of::<FactCacheEntry>())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "reference fact cache batch",
            })?;

        budget.check_bytes(merged_bytes)?;
        let mut merged = Vec::new();
        merged.try_reserve_exact(final_count).map_err(|source| {
            ReferenceGraphError::Allocation {
                resource: "reference fact cache batch",
                requested: final_count,
                unit: super::ReferenceAllocationUnit::Elements,
                source,
            }
        })?;
        budget.consume_bytes(merged_bytes)?;
        state.commit_sweep(scanned, budget)?;

        let existing = std::mem::take(&mut state.facts);
        let mut existing = existing.into_iter();
        let mut candidates = candidates.into_iter();
        let mut current = existing.next();
        let mut candidate = candidates.next();
        loop {
            match (current.take(), candidate.take()) {
                (Some(mut existing_entry), Some(candidate_entry)) => {
                    let candidate_key = FactCacheKey::new(candidate_entry.fingerprint);
                    match existing_entry.key.cmp(&candidate_key) {
                        std::cmp::Ordering::Less => {
                            if existing_entry.owner.strong_count() != 0 {
                                merged.push(existing_entry);
                            }
                            current = existing.next();
                            candidate = Some(candidate_entry);
                        }
                        std::cmp::Ordering::Greater => {
                            merged.push(cache_entry(candidate_entry));
                            current = Some(existing_entry);
                            candidate = candidates.next();
                        }
                        std::cmp::Ordering::Equal => {
                            if existing_entry.owner.strong_count() == 0 {
                                merged.push(cache_entry(candidate_entry));
                            } else {
                                existing_entry.owner =
                                    Arc::downgrade(candidate_entry.owner.image().backing());
                                merged.push(existing_entry);
                            }
                            current = existing.next();
                            candidate = candidates.next();
                        }
                    }
                }
                (Some(existing_entry), None) => {
                    if existing_entry.owner.strong_count() != 0 {
                        merged.push(existing_entry);
                    }
                    for existing_entry in existing {
                        if existing_entry.owner.strong_count() != 0 {
                            merged.push(existing_entry);
                        }
                    }
                    break;
                }
                (None, Some(candidate_entry)) => {
                    merged.push(cache_entry(candidate_entry));
                    merged.extend(candidates.map(cache_entry));
                    break;
                }
                (None, None) => break,
            }
        }
        state.facts = merged;
        Ok(())
    }

    pub(crate) fn graph(
        &self,
        revision: WorkspaceRevision,
        options: ReferenceGraphBuildOptions,
    ) -> Result<Option<Arc<ReferenceIndex>>, ReferenceGraphError> {
        let state = self
            .state
            .lock()
            .map_err(|_| ReferenceGraphError::CachePoisoned)?;
        let key = GraphCacheKey { revision, options };
        let Ok(position) = state.graphs.binary_search_by_key(&key, |entry| entry.key) else {
            return Ok(None);
        };
        Ok(state.graphs[position].graph.upgrade())
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

fn cache_entry(candidate: FactCacheCandidate) -> FactCacheEntry {
    FactCacheEntry {
        key: FactCacheKey::new(candidate.fingerprint),
        owner: Arc::downgrade(candidate.owner.image().backing()),
        occurrences: candidate.occurrences,
    }
}

struct FactMergePlan {
    final_count: usize,
    #[cfg(test)]
    key_comparisons: usize,
}

fn plan_fact_merge(
    existing: &[FactCacheEntry],
    candidates: &[FactCacheCandidate],
) -> Result<FactMergePlan, ReferenceGraphError> {
    let mut final_count = 0_usize;
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
                if existing[existing_index].owner.strong_count() != 0 {
                    final_count = checked_fact_count(final_count)?;
                }
                existing_index += 1;
            }
            std::cmp::Ordering::Greater => {
                final_count = checked_fact_count(final_count)?;
                candidate_index += 1;
            }
            std::cmp::Ordering::Equal => {
                final_count = checked_fact_count(final_count)?;
                existing_index += 1;
                candidate_index += 1;
            }
        }
    }
    for entry in &existing[existing_index..] {
        if entry.owner.strong_count() != 0 {
            final_count = checked_fact_count(final_count)?;
        }
    }
    final_count = final_count
        .checked_add(candidates.len().saturating_sub(candidate_index))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "reference fact cache batch",
        })?;

    Ok(FactMergePlan {
        final_count,
        #[cfg(test)]
        key_comparisons,
    })
}

fn checked_fact_count(count: usize) -> Result<usize, ReferenceGraphError> {
    count
        .checked_add(1)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "reference fact cache batch",
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

    use crate::workspace::{AssetWorkspace, reference_view_parts};
    use unity_asset_core::{AssetLoadLimits, SourceKind};

    use super::*;

    fn source_owner() -> Arc<SourceEntry> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owner.prefab");
        fs::write(
            &path,
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Owner\n",
        )
        .unwrap();
        let mut workspace = AssetWorkspace::new().unwrap();
        let source = workspace
            .load_path(&path, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        Arc::clone(
            reference_view_parts(&snapshot)
                .state
                .store()
                .get(source)
                .unwrap(),
        )
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
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.prefab");
        fs::write(
            &path,
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Source\n",
        )
        .unwrap();
        let mut workspace = AssetWorkspace::new().unwrap();
        let source = workspace
            .load_path(&path, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        let parts = reference_view_parts(&snapshot);
        let owner = Arc::clone(parts.state.store().get(source).unwrap());
        let fingerprint = owner.image().fingerprint();
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
                    owner,
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

        let directory = tempfile::tempdir().unwrap();
        let mut workspace = AssetWorkspace::new().unwrap();
        let mut sources = Vec::new();
        for index in 0..CACHED_SOURCES + 2 {
            let path = directory.path().join(format!("live-{index}.prefab"));
            fs::write(
                &path,
                format!(
                    "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &{}\nGameObject:\n  m_Name: Live{index}\n",
                    index + 1
                ),
            )
            .unwrap();
            sources.push(
                workspace
                    .load_path(&path, &mut AssetLoadBudget::default())
                    .unwrap(),
            );
        }

        let doomed = sources[CACHED_SOURCES + 1];
        let snapshot = workspace.snapshot();
        let parts = reference_view_parts(&snapshot);
        let cached = sources[..CACHED_SOURCES]
            .iter()
            .map(|source| Arc::clone(parts.state.store().get(*source).unwrap()))
            .collect::<Vec<_>>();
        let next = Arc::clone(parts.state.store().get(sources[CACHED_SOURCES]).unwrap());
        let doomed_owner = Arc::clone(parts.state.store().get(doomed).unwrap());
        drop(snapshot);

        let store = ReferenceStore::new();
        let candidate = Arc::new(SourceReferenceOccurrences {
            occurrences: Box::new([]),
            diagnostics: Box::new([]),
            object_count: 1,
            complete: true,
        });
        let mut candidates = cached
            .iter()
            .map(|owner| FactCacheCandidate {
                fingerprint: owner.image().fingerprint(),
                owner: Arc::clone(owner),
                occurrences: Arc::clone(&candidate),
            })
            .collect::<Vec<_>>();
        candidates.push(FactCacheCandidate {
            fingerprint: doomed_owner.image().fingerprint(),
            owner: Arc::clone(&doomed_owner),
            occurrences: Arc::clone(&candidate),
        });
        candidates.sort_unstable_by_key(|candidate| candidate.fingerprint);
        store
            .publish_facts_batch(candidates, &mut AssetLoadBudget::default())
            .unwrap();
        drop(doomed_owner);
        workspace
            .unload_source(doomed, &mut AssetLoadBudget::default())
            .unwrap();

        let mut tiny = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let first = &cached[0];
        let hit = store
            .facts(first.image().fingerprint(), first)
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&hit, &candidate));

        assert!(matches!(
            store.publish_facts_batch(
                vec![FactCacheCandidate {
                    fingerprint: next.image().fingerprint(),
                    owner: Arc::clone(&next),
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
        assert!(
            cache
                .facts
                .iter()
                .any(|entry| entry.owner.strong_count() == 0)
        );
        assert!(
            cache
                .facts
                .binary_search_by_key(&FactCacheKey::new(next.image().fingerprint()), |entry| {
                    entry.key
                })
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
                owner: Arc::clone(&owner),
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
            owner: Arc::clone(&owner),
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
                        owner: Arc::clone(&owner),
                        occurrences: Arc::clone(&old_live),
                    },
                    FactCacheCandidate {
                        fingerprint: keys[1],
                        owner: Arc::clone(&owner),
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
            let temporary = Arc::<[u8]>::from([]);
            state.facts[dead].owner = Arc::downgrade(&temporary);
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
                        owner: Arc::clone(&owner),
                        occurrences: replacement_live,
                    },
                    FactCacheCandidate {
                        fingerprint: keys[1],
                        owner: Arc::clone(&owner),
                        occurrences: Arc::clone(&replacement_dead),
                    },
                    FactCacheCandidate {
                        fingerprint: keys[2],
                        owner: Arc::clone(&owner),
                        occurrences: Arc::clone(&inserted),
                    },
                ],
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert!(Arc::ptr_eq(
            &store.facts(keys[0], &owner).unwrap().unwrap(),
            &old_live
        ));
        assert!(Arc::ptr_eq(
            &store.facts(keys[1], &owner).unwrap().unwrap(),
            &replacement_dead
        ));
        assert!(Arc::ptr_eq(
            &store.facts(keys[2], &owner).unwrap().unwrap(),
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
                owner: Arc::clone(&owner),
                occurrences: Arc::clone(&shared),
            })
            .collect::<Vec<_>>();
        let additions = keys
            .iter()
            .skip(1)
            .step_by(2)
            .map(|fingerprint| FactCacheCandidate {
                fingerprint: *fingerprint,
                owner: Arc::clone(&owner),
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
