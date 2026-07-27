//! External TypeTree registry (UnityPy TPK-like fallback).
//!
//! Unity assets can be built with stripped TypeTrees (`enableTypeTree = false`). In those cases,
//! consumers may still want a best-effort parser by supplying an external registry of TypeTrees.
//!
//! This module provides an injectable registry abstraction and a simple JSON-backed implementation.

use crate::typetree::{TypeTree, TypeTreeNode};
use crate::{error::BinaryError, error::Result};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;
use std::fmt::Write as _;
use std::io::Read;
use std::mem::size_of;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use unity_asset_core::{AssetLoadBudget, BudgetError, DigestV1};

pub trait TypeTreeRegistry: Send + Sync + std::fmt::Debug {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>>;

    /// Returns a stable digest when this registry can participate in revision identity.
    ///
    /// Mutable or callback-backed registries should keep the default `None`. Revisioned
    /// workspaces freeze every lookup they retain into an immutable registry that supplies a
    /// digest, so a snapshot never depends on unidentifiable external state.
    fn semantic_digest(&self) -> Option<DigestV1> {
        None
    }

    /// Resolve a script type tree (e.g. MonoBehaviour) using the script's 16-byte ID.
    ///
    /// Unity stores script types as `class_id=114` with a `script_id` value in `SerializedType`.
    /// UnityPy uses `TypeTreeGeneratorAPI` to produce a per-script TypeTree; this hook lets callers
    /// provide equivalent data via registries.
    fn resolve_script(
        &self,
        _unity_version: &str,
        _class_id: i32,
        _script_id: [u8; 16],
    ) -> Option<Arc<TypeTree>> {
        None
    }
}

/// A registry that resolves by trying multiple registries in order (first match wins).
#[derive(Debug, Default, Clone)]
pub struct CompositeTypeTreeRegistry {
    registries: Vec<Arc<dyn TypeTreeRegistry>>,
}

impl CompositeTypeTreeRegistry {
    pub fn new(registries: Vec<Arc<dyn TypeTreeRegistry>>) -> Self {
        Self { registries }
    }

    /// Loads registry files in priority order under one caller-owned budget.
    ///
    /// `.tpk` paths use [`super::tpk::TpkTypeTreeRegistry`]; every other path uses the
    /// strict JSON registry format. Empty and single-path inputs avoid a composite allocation.
    /// Multiple paths account for the priority table, each concrete `Arc` allocation, and the
    /// final composite `Arc` before allocating them.
    pub fn from_paths<P: AsRef<Path>>(
        paths: &[P],
        budget: &mut AssetLoadBudget,
    ) -> Result<Option<Arc<dyn TypeTreeRegistry>>> {
        if paths.is_empty() {
            return Ok(None);
        }

        let member_count =
            u64::try_from(paths.len()).map_err(|_| BudgetError::ArithmeticOverflow {
                resource: "members",
            })?;
        budget.consume_members(member_count)?;

        if let [path] = paths {
            return load_type_tree_registry_path(path.as_ref(), budget).map(Some);
        }

        let mut registries = Vec::new();
        reserve_exact_budgeted_vec(
            &mut registries,
            paths.len(),
            budget,
            "TypeTree registry table",
        )?;

        for path in paths {
            registries.push(load_type_tree_registry_path(path.as_ref(), budget)?);
        }

        consume_arc_allocation::<Self>(budget)?;
        Ok(Some(Arc::new(Self::new(registries))))
    }

    /// Composes existing registries in priority order under `budget`.
    ///
    /// Empty and single-item inputs are allocation-free. Multiple items account for the cloned
    /// registry table and the composite `Arc` before publishing the result.
    pub fn compose(
        registries: &[Arc<dyn TypeTreeRegistry>],
        budget: &mut AssetLoadBudget,
    ) -> Result<Option<Arc<dyn TypeTreeRegistry>>> {
        match registries {
            [] => Ok(None),
            [registry] => Ok(Some(registry.clone())),
            _ => {
                let member_count = u64::try_from(registries.len()).map_err(|_| {
                    BudgetError::ArithmeticOverflow {
                        resource: "members",
                    }
                })?;
                budget.consume_members(member_count)?;

                let mut owned = Vec::new();
                reserve_exact_budgeted_vec(
                    &mut owned,
                    registries.len(),
                    budget,
                    "composite TypeTree registry table",
                )?;
                owned.extend(registries.iter().cloned());

                consume_arc_allocation::<Self>(budget)?;
                Ok(Some(Arc::new(Self::new(owned))))
            }
        }
    }

    pub fn push(&mut self, registry: Arc<dyn TypeTreeRegistry>) {
        self.registries.push(registry);
    }

    pub fn extend(&mut self, registries: impl IntoIterator<Item = Arc<dyn TypeTreeRegistry>>) {
        self.registries.extend(registries);
    }

    pub fn is_empty(&self) -> bool {
        self.registries.is_empty()
    }
}

fn load_type_tree_registry_path(
    path: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<Arc<dyn TypeTreeRegistry>> {
    let is_tpk = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("tpk"));
    if is_tpk {
        let registry = super::tpk::TpkTypeTreeRegistry::from_path(path, budget)?;
        consume_arc_allocation::<super::tpk::TpkTypeTreeRegistry>(budget)?;
        Ok(Arc::new(registry))
    } else {
        let registry = JsonTypeTreeRegistry::from_path(path, budget)?;
        consume_arc_allocation::<JsonTypeTreeRegistry>(budget)?;
        Ok(Arc::new(registry))
    }
}

impl TypeTreeRegistry for CompositeTypeTreeRegistry {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        for r in &self.registries {
            if let Some(t) = r.resolve(unity_version, class_id) {
                return Some(t);
            }
        }
        None
    }

    fn resolve_script(
        &self,
        unity_version: &str,
        class_id: i32,
        script_id: [u8; 16],
    ) -> Option<Arc<TypeTree>> {
        for r in &self.registries {
            if let Some(t) = r.resolve_script(unity_version, class_id, script_id) {
                return Some(t);
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
enum VersionSelector {
    Any,
    Exact(String),
    Prefix(String),
}

#[derive(Debug, Clone)]
struct RegistryEntry {
    selector: VersionSelector,
    tree: Arc<TypeTree>,
}

/// A vector whose geometric logical capacity is charged independently of allocator spare capacity.
#[derive(Debug)]
struct BudgetedVec<T> {
    values: Vec<T>,
    accounted_capacity: usize,
}

impl<T> Default for BudgetedVec<T> {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            accounted_capacity: 0,
        }
    }
}

impl<T: Clone> Clone for BudgetedVec<T> {
    fn clone(&self) -> Self {
        let values = self.values.clone();
        let accounted_capacity = values.capacity();
        Self {
            values,
            accounted_capacity,
        }
    }
}

impl<T> BudgetedVec<T> {
    fn reserve_budgeted(
        &mut self,
        additional: usize,
        budget: &mut AssetLoadBudget,
        label: &str,
    ) -> Result<()> {
        let required = self
            .values
            .len()
            .checked_add(additional)
            .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
        if required <= self.accounted_capacity {
            return Ok(());
        }

        let target_capacity = required
            .checked_next_power_of_two()
            .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
        let added_capacity = target_capacity - self.accounted_capacity;
        let allocation = added_capacity
            .checked_mul(size_of::<T>())
            .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
        let allocation = u64::try_from(allocation)
            .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
        budget.check_bytes(allocation)?;

        let reserve = target_capacity - self.values.len();
        self.values.try_reserve_exact(reserve).map_err(|error| {
            BinaryError::memory_error(format!("Failed to reserve {label}: {error}"))
        })?;
        budget.consume_bytes(allocation)?;
        self.accounted_capacity = target_capacity;
        Ok(())
    }

    fn push_unbudgeted(&mut self, value: T) {
        self.values.push(value);
        self.accounted_capacity = self.values.capacity();
    }

    fn insert_unbudgeted(&mut self, index: usize, value: T) {
        self.values.insert(index, value);
        self.accounted_capacity = self.values.capacity();
    }

    fn as_slice(&self) -> &[T] {
        &self.values
    }

    fn into_vec(self) -> Vec<T> {
        self.values
    }
}

#[derive(Debug, Clone)]
struct RegistryBucket<K> {
    key: K,
    entries: BudgetedVec<RegistryEntry>,
}

impl<K> RegistryBucket<K> {
    fn new(key: K) -> Self {
        Self {
            key,
            entries: BudgetedVec::default(),
        }
    }
}

#[derive(Debug, Clone)]
struct RegistryTable<K> {
    buckets: BudgetedVec<RegistryBucket<K>>,
}

impl<K> Default for RegistryTable<K> {
    fn default() -> Self {
        Self {
            buckets: BudgetedVec::default(),
        }
    }
}

impl<K: Copy + Ord> RegistryTable<K> {
    fn entries(&self, key: K) -> Option<&[RegistryEntry]> {
        let index = self
            .buckets
            .values
            .binary_search_by_key(&key, |bucket| bucket.key)
            .ok()?;
        Some(self.buckets.values[index].entries.as_slice())
    }

    fn entries_mut_unbudgeted(&mut self, key: K) -> &mut BudgetedVec<RegistryEntry> {
        let index = match self
            .buckets
            .values
            .binary_search_by_key(&key, |bucket| bucket.key)
        {
            Ok(index) => index,
            Err(index) => {
                self.buckets
                    .insert_unbudgeted(index, RegistryBucket::new(key));
                index
            }
        };
        &mut self.buckets.values[index].entries
    }

    fn append_entries_mut_budgeted(
        &mut self,
        key: K,
        budget: &mut AssetLoadBudget,
        label: &str,
    ) -> Result<&mut BudgetedVec<RegistryEntry>> {
        if let Some(last_key) = self.buckets.values.last().map(|bucket| bucket.key) {
            match last_key.cmp(&key) {
                std::cmp::Ordering::Equal => {
                    return self
                        .buckets
                        .values
                        .last_mut()
                        .map(|bucket| &mut bucket.entries)
                        .ok_or_else(|| {
                            BinaryError::invalid_data(
                                "Budgeted registry table lost its last bucket",
                            )
                        });
                }
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Greater => {
                    return Err(BinaryError::invalid_data(
                        "Budgeted registry entries must be grouped in ascending key order",
                    ));
                }
            }
        }

        self.buckets.reserve_budgeted(1, budget, label)?;
        self.buckets.values.push(RegistryBucket::new(key));
        self.buckets
            .values
            .last_mut()
            .map(|bucket| &mut bucket.entries)
            .ok_or_else(|| BinaryError::invalid_data("Budgeted registry bucket was not appended"))
    }
}

/// A simple in-memory registry keyed by Unity class ID.
#[derive(Debug, Default, Clone)]
pub struct InMemoryTypeTreeRegistry {
    by_class_id: RegistryTable<i32>,
    by_script_id: RegistryTable<[u8; 16]>,
}

impl InMemoryTypeTreeRegistry {
    pub fn insert_any(&mut self, class_id: i32, tree: TypeTree) {
        self.insert_internal(class_id, VersionSelector::Any, tree);
    }

    pub fn insert_exact(&mut self, unity_version: String, class_id: i32, tree: TypeTree) {
        self.insert_internal(class_id, VersionSelector::Exact(unity_version), tree);
    }

    pub fn insert_prefix(&mut self, unity_version_prefix: String, class_id: i32, tree: TypeTree) {
        self.insert_internal(
            class_id,
            VersionSelector::Prefix(unity_version_prefix),
            tree,
        );
    }

    pub fn insert_script_any(&mut self, script_id: [u8; 16], tree: TypeTree) {
        self.insert_script_internal(script_id, VersionSelector::Any, tree);
    }

    pub fn insert_script_exact(
        &mut self,
        unity_version: String,
        script_id: [u8; 16],
        tree: TypeTree,
    ) {
        self.insert_script_internal(script_id, VersionSelector::Exact(unity_version), tree);
    }

    pub fn insert_script_prefix(
        &mut self,
        unity_version_prefix: String,
        script_id: [u8; 16],
        tree: TypeTree,
    ) {
        self.insert_script_internal(
            script_id,
            VersionSelector::Prefix(unity_version_prefix),
            tree,
        );
    }

    fn insert_internal(&mut self, class_id: i32, selector: VersionSelector, tree: TypeTree) {
        self.by_class_id
            .entries_mut_unbudgeted(class_id)
            .push_unbudgeted(RegistryEntry {
                selector,
                tree: Arc::new(tree),
            });
    }

    fn insert_script_internal(
        &mut self,
        script_id: [u8; 16],
        selector: VersionSelector,
        tree: TypeTree,
    ) {
        self.by_script_id
            .entries_mut_unbudgeted(script_id)
            .push_unbudgeted(RegistryEntry {
                selector,
                tree: Arc::new(tree),
            });
    }
}

impl TypeTreeRegistry for InMemoryTypeTreeRegistry {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        let entries = self.by_class_id.entries(class_id)?;

        // 1) exact match
        for e in entries {
            if matches!(&e.selector, VersionSelector::Exact(v) if v == unity_version) {
                return Some(e.tree.clone());
            }
        }

        // 2) best (longest) prefix match
        let mut best: Option<(&RegistryEntry, usize)> = None;
        for e in entries {
            let VersionSelector::Prefix(prefix) = &e.selector else {
                continue;
            };
            if unity_version.starts_with(prefix) {
                let len = prefix.len();
                match best {
                    Some((_prev, prev_len)) if prev_len >= len => {}
                    _ => best = Some((e, len)),
                }
            }
        }
        if let Some((e, _)) = best {
            return Some(e.tree.clone());
        }

        // 3) any
        for e in entries {
            if matches!(e.selector, VersionSelector::Any) {
                return Some(e.tree.clone());
            }
        }

        None
    }

    fn resolve_script(
        &self,
        unity_version: &str,
        _class_id: i32,
        script_id: [u8; 16],
    ) -> Option<Arc<TypeTree>> {
        let entries = self.by_script_id.entries(script_id)?;

        // 1) exact match
        for e in entries {
            if matches!(&e.selector, VersionSelector::Exact(v) if v == unity_version) {
                return Some(e.tree.clone());
            }
        }

        // 2) best (longest) prefix match
        let mut best: Option<(&RegistryEntry, usize)> = None;
        for e in entries {
            let VersionSelector::Prefix(prefix) = &e.selector else {
                continue;
            };
            if unity_version.starts_with(prefix) {
                let len = prefix.len();
                match best {
                    Some((_prev, prev_len)) if prev_len >= len => {}
                    _ => best = Some((e, len)),
                }
            }
        }
        if let Some((e, _)) = best {
            return Some(e.tree.clone());
        }

        // 3) any
        for e in entries {
            if matches!(e.selector, VersionSelector::Any) {
                return Some(e.tree.clone());
            }
        }

        None
    }
}

struct ParsedRegistryFile {
    schema: u32,
    inner: InMemoryTypeTreeRegistry,
}

struct PendingRegistryEntry {
    unity_version: Option<String>,
    class_id: i32,
    script_id: Option<[u8; 16]>,
    type_tree: TypeTree,
    ordinal: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RegistryLookupKey {
    Class(i32),
    Script([u8; 16]),
}

impl PendingRegistryEntry {
    fn lookup_key(&self) -> RegistryLookupKey {
        self.script_id.map_or(
            RegistryLookupKey::Class(self.class_id),
            RegistryLookupKey::Script,
        )
    }
}

/// JSON-backed TypeTree registry.
///
/// Format:
/// ```json
/// { "schema": 1, "entries": [ { "unity_version": "2020.3.*", "class_id": 28, "type_tree": { ... } } ] }
/// ```
#[derive(Debug, Default, Clone)]
pub struct JsonTypeTreeRegistry {
    inner: InMemoryTypeTreeRegistry,
}

const MAX_JSON_TYPE_TREE_DEPTH: u32 = 59;
/// Maximum encoded size accepted by the external JSON TypeTree registry contract.
///
/// TPK registries remain the compact format for larger catalogs. Keeping a contract-level cap
/// here prevents a permissive caller budget from turning an untrusted JSON file into a GiB-scale
/// retained input and parser workload.
pub const MAX_JSON_TYPE_TREE_REGISTRY_BYTES: usize = 128 * 1024 * 1024;
const JSON_PARSER_WORK_MULTIPLIER: u64 = 6;
const JSON_ERROR_DIAGNOSTIC_BYTES: usize = 4 * 1024;

fn json_parser_chunk_work_bytes(encoded_len: usize) -> Result<u64> {
    let encoded_len = u64::try_from(encoded_len)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    encoded_len
        .checked_mul(JSON_PARSER_WORK_MULTIPLIER)
        .ok_or_else(|| BudgetError::ArithmeticOverflow { resource: "bytes" }.into())
}

struct BoundedDiagnostic {
    message: String,
    max_len: usize,
}

impl fmt::Write for BoundedDiagnostic {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let Some(required) = self.message.len().checked_add(value.len()) else {
            return Err(fmt::Error);
        };
        if required > self.max_len {
            return Err(fmt::Error);
        }
        self.message.push_str(value);
        Ok(())
    }
}

fn invalid_registry_json_error(error: serde_json::Error) -> Result<BinaryError> {
    let mut message = String::new();
    message
        .try_reserve_exact(JSON_ERROR_DIAGNOSTIC_BYTES)
        .map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve registry JSON diagnostic: {error}"
            ))
        })?;
    let mut diagnostic = BoundedDiagnostic {
        message,
        max_len: JSON_ERROR_DIAGNOSTIC_BYTES,
    };
    if write!(&mut diagnostic, "Invalid registry JSON: {error}").is_err() {
        diagnostic.message.clear();
        diagnostic
            .message
            .push_str("Invalid registry JSON: diagnostic exceeds 4096 bytes");
    }
    Ok(BinaryError::invalid_data(diagnostic.message))
}

impl JsonTypeTreeRegistry {
    pub fn from_reader(mut reader: impl Read, budget: &mut AssetLoadBudget) -> Result<Self> {
        budget.consume_bytes(
            u64::try_from(JSON_ERROR_DIAGNOSTIC_BYTES)
                .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?,
        )?;

        let encoded =
            read_registry_json_input(&mut reader, budget, MAX_JSON_TYPE_TREE_REGISTRY_BYTES)?;

        let mut context = JsonLoadContext {
            budget,
            failure: None,
        };
        let parsed_result = {
            let mut deserializer = serde_json::Deserializer::from_slice(encoded.as_slice());
            let parsed = RegistryFileSeed {
                context: &mut context,
            }
            .deserialize(&mut deserializer);
            match parsed {
                Ok(parsed) => deserializer.end().map(|()| parsed),
                Err(error) => Err(error),
            }
        };
        let parsed = match parsed_result {
            Ok(parsed) => parsed,
            Err(error) => {
                if let Some(error) = context.failure.take() {
                    return Err(error);
                }
                return Err(invalid_registry_json_error(error)?);
            }
        };
        if parsed.schema != 1 && parsed.schema != 2 {
            return Err(BinaryError::invalid_data(format!(
                "Unsupported registry schema: {}",
                parsed.schema
            )));
        }

        Ok(Self {
            inner: parsed.inner,
        })
    }

    pub fn from_path(path: impl AsRef<Path>, budget: &mut AssetLoadBudget) -> Result<Self> {
        let mut file = std::fs::File::open(path.as_ref())?;
        Self::from_reader(&mut file, budget)
    }
}

fn read_registry_json_input(
    reader: &mut impl Read,
    budget: &mut AssetLoadBudget,
    max_encoded_bytes: usize,
) -> Result<BudgetedVec<u8>> {
    debug_assert!(max_encoded_bytes > 0);
    let mut encoded = BudgetedVec::default();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let requested =
            encoded
                .values
                .len()
                .checked_add(read)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "registry JSON input",
                })?;
        if requested > max_encoded_bytes {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "TypeTree registry JSON input requested {requested} bytes, limit {max_encoded_bytes}"
            )));
        }
        // Reserve Serde's input-proportional scratch before retaining the corresponding input.
        budget.consume_bytes(json_parser_chunk_work_bytes(read)?)?;
        encoded.reserve_budgeted(read, budget, "registry JSON input")?;
        encoded.values.extend_from_slice(&buffer[..read]);
    }
    Ok(encoded)
}

fn reserve_exact_budgeted_vec<T>(
    values: &mut Vec<T>,
    additional: usize,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<()> {
    let allocation = additional
        .checked_mul(size_of::<T>())
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let allocation = u64::try_from(allocation)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    budget.check_bytes(allocation)?;
    values.try_reserve_exact(additional).map_err(|error| {
        BinaryError::memory_error(format!("Failed to reserve {label}: {error}"))
    })?;
    budget.consume_bytes(allocation)?;
    Ok(())
}

fn reserve_budgeted_string(
    value: &mut String,
    capacity: usize,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<()> {
    let allocation = u64::try_from(capacity)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    budget.check_bytes(allocation)?;
    value.try_reserve_exact(capacity).map_err(|error| {
        BinaryError::memory_error(format!("Failed to reserve {label}: {error}"))
    })?;
    budget.consume_bytes(allocation)?;
    Ok(())
}

#[repr(C)]
struct ArcAllocation<T> {
    strong: AtomicUsize,
    weak: AtomicUsize,
    value: T,
}

fn consume_arc_allocation<T>(budget: &mut AssetLoadBudget) -> Result<()> {
    let allocation = u64::try_from(size_of::<ArcAllocation<T>>())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    budget.consume_bytes(allocation)?;
    Ok(())
}

fn selector_from_version(version: Option<String>) -> VersionSelector {
    match version {
        None => VersionSelector::Any,
        Some(value) if value.is_empty() => VersionSelector::Any,
        Some(mut value) if value.ends_with('*') => {
            value.pop();
            VersionSelector::Prefix(value)
        }
        Some(value) => VersionSelector::Exact(value),
    }
}

fn build_budgeted_registry(
    mut entries: BudgetedVec<PendingRegistryEntry>,
    budget: &mut AssetLoadBudget,
) -> Result<InMemoryTypeTreeRegistry> {
    entries
        .values
        .sort_unstable_by_key(|entry| (entry.lookup_key(), entry.ordinal));

    let mut inner = InMemoryTypeTreeRegistry::default();
    for entry in entries.into_vec() {
        append_budgeted_registry_entry(&mut inner, entry, budget)?;
    }
    Ok(inner)
}

fn append_budgeted_registry_entry(
    inner: &mut InMemoryTypeTreeRegistry,
    entry: PendingRegistryEntry,
    budget: &mut AssetLoadBudget,
) -> Result<()> {
    let lookup_key = entry.lookup_key();
    let PendingRegistryEntry {
        unity_version,
        type_tree,
        class_id: _,
        script_id: _,
        ordinal: _,
    } = entry;
    let selector = selector_from_version(unity_version);

    let entries =
        match lookup_key {
            RegistryLookupKey::Class(class_id) => inner.by_class_id.append_entries_mut_budgeted(
                class_id,
                budget,
                "class registry lookup table",
            )?,
            RegistryLookupKey::Script(script_id) => inner
                .by_script_id
                .append_entries_mut_budgeted(script_id, budget, "script registry lookup table")?,
        };
    entries.reserve_budgeted(1, budget, "registry entry table")?;
    consume_arc_allocation::<TypeTree>(budget)?;
    entries.values.push(RegistryEntry {
        selector,
        tree: Arc::new(type_tree),
    });

    Ok(())
}

struct JsonLoadContext<'a> {
    budget: &'a mut AssetLoadBudget,
    failure: Option<BinaryError>,
}

impl JsonLoadContext<'_> {
    fn capture<T, E: de::Error>(&mut self, result: Result<T>) -> std::result::Result<T, E> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                if self.failure.is_none() {
                    self.failure = Some(error);
                }
                Err(E::custom(
                    "registry JSON resource limit or allocation failure",
                ))
            }
        }
    }

    fn consume_entries<E: de::Error>(&mut self, amount: u64) -> std::result::Result<(), E> {
        let result = self.budget.consume_entries(amount).map_err(Into::into);
        self.capture(result)
    }

    fn consume_members<E: de::Error>(&mut self, amount: u64) -> std::result::Result<(), E> {
        let result = self.budget.consume_members(amount).map_err(Into::into);
        self.capture(result)
    }

    fn observe_depth<E: de::Error>(&mut self, depth: u32) -> std::result::Result<(), E> {
        let result = self.budget.observe_depth(depth).map_err(Into::into);
        self.capture(result)
    }

    fn consume_bytes<E: de::Error>(&mut self, amount: u64) -> std::result::Result<(), E> {
        let result = self.budget.consume_bytes(amount).map_err(Into::into);
        self.capture(result)
    }

    fn reserve_vec<T, E: de::Error>(
        &mut self,
        values: &mut BudgetedVec<T>,
        additional: usize,
        label: &str,
    ) -> std::result::Result<(), E> {
        let result = values.reserve_budgeted(additional, self.budget, label);
        self.capture(result)
    }

    fn copy_string<E: de::Error>(
        &mut self,
        value: &str,
        label: &str,
    ) -> std::result::Result<String, E> {
        let mut owned = String::new();
        let result = reserve_budgeted_string(&mut owned, value.len(), self.budget, label);
        self.capture(result)?;
        owned.push_str(value);
        Ok(owned)
    }
}

struct BudgetedStringSeed<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
    label: &'static str,
}

impl<'de> DeserializeSeed<'de> for BudgetedStringSeed<'_, '_> {
    type Value = String;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(BudgetedStringVisitor {
            context: self.context,
            label: self.label,
        })
    }
}

struct BudgetedStringVisitor<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
    label: &'static str,
}

impl<'de> Visitor<'de> for BudgetedStringVisitor<'_, '_> {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a UTF-8 string")
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.context.copy_string(value, self.label)
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.context.copy_string(value, self.label)
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        let allocation = u64::try_from(value.capacity())
            .map_err(|_| E::custom(format_args!("{} capacity does not fit u64", self.label)))?;
        self.context.consume_bytes(allocation)?;
        Ok(value)
    }
}

const REGISTRY_FILE_FIELDS: &[&str] = &["schema", "entries"];

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum RegistryFileField {
    Schema,
    Entries,
}

struct RegistryFileSeed<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
}

impl<'de> DeserializeSeed<'de> for RegistryFileSeed<'_, '_> {
    type Value = ParsedRegistryFile;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_struct(
            "JsonTypeTreeRegistry",
            REGISTRY_FILE_FIELDS,
            RegistryFileVisitor {
                context: self.context,
            },
        )
    }
}

struct RegistryFileVisitor<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
}

impl<'de> Visitor<'de> for RegistryFileVisitor<'_, '_> {
    type Value = ParsedRegistryFile;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a strict TypeTree registry object")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut schema = None;
        let mut entries_seen = false;
        let mut entries = BudgetedVec::default();

        while let Some(field) = map.next_key::<RegistryFileField>()? {
            match field {
                RegistryFileField::Schema => {
                    set_once(&mut schema, map.next_value()?, "schema")?;
                }
                RegistryFileField::Entries => {
                    if entries_seen {
                        return Err(de::Error::duplicate_field("entries"));
                    }
                    entries_seen = true;
                    map.next_value_seed(RegistryEntriesSeed {
                        context: &mut *self.context,
                        entries: &mut entries,
                    })?;
                }
            }
        }

        if !entries_seen {
            return Err(de::Error::missing_field("entries"));
        }
        let schema = required(schema, "schema")?;
        let build_result = build_budgeted_registry(entries, self.context.budget);
        let inner = self.context.capture(build_result)?;
        Ok(ParsedRegistryFile { schema, inner })
    }
}

struct RegistryEntriesSeed<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
    entries: &'a mut BudgetedVec<PendingRegistryEntry>,
}

impl<'de> DeserializeSeed<'de> for RegistryEntriesSeed<'_, '_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(RegistryEntriesVisitor {
            context: self.context,
            entries: self.entries,
        })
    }
}

struct RegistryEntriesVisitor<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
    entries: &'a mut BudgetedVec<PendingRegistryEntry>,
}

impl<'de> Visitor<'de> for RegistryEntriesVisitor<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of TypeTree registry entries")
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence
            .next_element_seed(RegistryEntryElementSeed {
                context: &mut *self.context,
                entries: &mut *self.entries,
            })?
            .is_some()
        {}
        Ok(())
    }
}

struct RegistryEntryElementSeed<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
    entries: &'a mut BudgetedVec<PendingRegistryEntry>,
}

impl<'de> DeserializeSeed<'de> for RegistryEntryElementSeed<'_, '_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.context.consume_entries(1)?;
        self.context.consume_members(1)?;
        self.context
            .reserve_vec(self.entries, 1, "pending registry entry table")?;
        let ordinal = self.entries.values.len();
        let mut entry = RegistryEntrySeed {
            context: &mut *self.context,
        }
        .deserialize(deserializer)?;
        entry.ordinal = ordinal;
        self.entries.values.push(entry);
        Ok(())
    }
}

const REGISTRY_ENTRY_FIELDS: &[&str] = &["unity_version", "class_id", "script_id", "type_tree"];

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum RegistryEntryField {
    UnityVersion,
    ClassId,
    ScriptId,
    TypeTree,
}

struct RegistryEntrySeed<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
}

impl<'de> DeserializeSeed<'de> for RegistryEntrySeed<'_, '_> {
    type Value = PendingRegistryEntry;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_struct(
            "JsonTypeTreeRegistryEntry",
            REGISTRY_ENTRY_FIELDS,
            RegistryEntryVisitor {
                context: self.context,
            },
        )
    }
}

struct RegistryEntryVisitor<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
}

impl<'de> Visitor<'de> for RegistryEntryVisitor<'_, '_> {
    type Value = PendingRegistryEntry;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a strict TypeTree registry entry")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut unity_version = None;
        let mut class_id = None;
        let mut script_id = None;
        let mut type_tree = None;

        while let Some(field) = map.next_key::<RegistryEntryField>()? {
            match field {
                RegistryEntryField::UnityVersion => {
                    if unity_version.is_some() {
                        return Err(de::Error::duplicate_field("unity_version"));
                    }
                    unity_version = Some(map.next_value_seed(OptionalStringSeed {
                        context: &mut *self.context,
                        label: "registry Unity version",
                    })?);
                }
                RegistryEntryField::ClassId => {
                    set_once(&mut class_id, map.next_value()?, "class_id")?;
                }
                RegistryEntryField::ScriptId => {
                    if script_id.is_some() {
                        return Err(de::Error::duplicate_field("script_id"));
                    }
                    let raw = map.next_value_seed(OptionalStringSeed {
                        context: &mut *self.context,
                        label: "registry script ID",
                    })?;
                    script_id = Some(match raw {
                        Some(raw) => Some(parse_hex_32_bytes(&raw).ok_or_else(|| {
                            de::Error::custom(
                                "script_id must contain exactly 32 hexadecimal digits",
                            )
                        })?),
                        None => None,
                    });
                }
                RegistryEntryField::TypeTree => {
                    if type_tree.is_some() {
                        return Err(de::Error::duplicate_field("type_tree"));
                    }
                    type_tree = Some(map.next_value_seed(TypeTreeSeed {
                        context: &mut *self.context,
                    })?);
                }
            }
        }

        Ok(PendingRegistryEntry {
            unity_version: unity_version.unwrap_or(None),
            class_id: required(class_id, "class_id")?,
            script_id: script_id.unwrap_or(None),
            type_tree: required(type_tree, "type_tree")?,
            ordinal: 0,
        })
    }
}

struct OptionalStringSeed<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
    label: &'static str,
}

impl<'de> DeserializeSeed<'de> for OptionalStringSeed<'_, '_> {
    type Value = Option<String>;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_option(OptionalStringVisitor {
            context: self.context,
            label: self.label,
        })
    }
}

struct OptionalStringVisitor<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
    label: &'static str,
}

impl<'de> Visitor<'de> for OptionalStringVisitor<'_, '_> {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a string or null")
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        BudgetedStringSeed {
            context: self.context,
            label: self.label,
        }
        .deserialize(deserializer)
        .map(Some)
    }
}

const TYPE_TREE_FIELDS: &[&str] = &[
    "nodes",
    "string_buffer",
    "version",
    "platform",
    "has_type_dependencies",
];

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum TypeTreeField {
    Nodes,
    StringBuffer,
    Version,
    Platform,
    HasTypeDependencies,
}

struct TypeTreeSeed<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
}

impl<'de> DeserializeSeed<'de> for TypeTreeSeed<'_, '_> {
    type Value = TypeTree;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_struct(
            "TypeTree",
            TYPE_TREE_FIELDS,
            TypeTreeVisitor {
                context: self.context,
            },
        )
    }
}

struct TypeTreeVisitor<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
}

impl<'de> Visitor<'de> for TypeTreeVisitor<'_, '_> {
    type Value = TypeTree;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a strict TypeTree object")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut nodes = None;
        let mut string_buffer = None;
        let mut version = None;
        let mut platform = None;
        let mut has_type_dependencies = None;

        while let Some(field) = map.next_key::<TypeTreeField>()? {
            match field {
                TypeTreeField::Nodes => {
                    if nodes.is_some() {
                        return Err(de::Error::duplicate_field("nodes"));
                    }
                    nodes = Some(map.next_value_seed(TypeTreeNodesSeed {
                        context: &mut *self.context,
                        placement: NodePlacement::Root,
                    })?);
                }
                TypeTreeField::StringBuffer => {
                    if string_buffer.is_some() {
                        return Err(de::Error::duplicate_field("string_buffer"));
                    }
                    string_buffer = Some(map.next_value_seed(ByteBufferSeed {
                        context: &mut *self.context,
                    })?);
                }
                TypeTreeField::Version => {
                    if version.replace(map.next_value()?).is_some() {
                        return Err(de::Error::duplicate_field("version"));
                    }
                }
                TypeTreeField::Platform => {
                    if platform.replace(map.next_value()?).is_some() {
                        return Err(de::Error::duplicate_field("platform"));
                    }
                }
                TypeTreeField::HasTypeDependencies => {
                    if has_type_dependencies.replace(map.next_value()?).is_some() {
                        return Err(de::Error::duplicate_field("has_type_dependencies"));
                    }
                }
            }
        }

        Ok(TypeTree {
            nodes: nodes
                .ok_or_else(|| de::Error::missing_field("nodes"))?
                .into_vec(),
            string_buffer: string_buffer
                .ok_or_else(|| de::Error::missing_field("string_buffer"))?
                .into_vec(),
            version: version.ok_or_else(|| de::Error::missing_field("version"))?,
            platform: platform.ok_or_else(|| de::Error::missing_field("platform"))?,
            has_type_dependencies: has_type_dependencies
                .ok_or_else(|| de::Error::missing_field("has_type_dependencies"))?,
        })
    }
}

#[derive(Clone, Copy)]
enum NodePlacement {
    Root,
    ChildOf(u32),
}

impl NodePlacement {
    fn depth(self) -> Result<u32> {
        match self {
            Self::Root => Ok(0),
            Self::ChildOf(parent) => {
                parent
                    .checked_add(1)
                    .ok_or(BinaryError::Budget(BudgetError::ArithmeticOverflow {
                        resource: "depth",
                    }))
            }
        }
    }
}

struct TypeTreeNodesSeed<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
    placement: NodePlacement,
}

impl<'de> DeserializeSeed<'de> for TypeTreeNodesSeed<'_, '_> {
    type Value = BudgetedVec<TypeTreeNode>;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(TypeTreeNodesVisitor {
            context: self.context,
            placement: self.placement,
        })
    }
}

struct TypeTreeNodesVisitor<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
    placement: NodePlacement,
}

impl<'de> Visitor<'de> for TypeTreeNodesVisitor<'_, '_> {
    type Value = BudgetedVec<TypeTreeNode>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of TypeTree nodes")
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut nodes = BudgetedVec::default();
        while sequence
            .next_element_seed(TypeTreeNodeElementSeed {
                context: &mut *self.context,
                nodes: &mut nodes,
                placement: self.placement,
            })?
            .is_some()
        {}
        Ok(nodes)
    }
}

struct TypeTreeNodeElementSeed<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
    nodes: &'a mut BudgetedVec<TypeTreeNode>,
    placement: NodePlacement,
}

impl<'de> DeserializeSeed<'de> for TypeTreeNodeElementSeed<'_, '_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        let depth_result = self.placement.depth();
        let depth = self.context.capture(depth_result)?;
        self.context.observe_depth(depth)?;
        if depth > MAX_JSON_TYPE_TREE_DEPTH {
            let error = BinaryError::Budget(BudgetError::Exceeded {
                resource: "json_typetree_depth",
                limit: u64::from(MAX_JSON_TYPE_TREE_DEPTH),
                requested: u64::from(depth),
            });
            self.context.capture(Err(error))?;
        }
        self.context.consume_entries(1)?;
        self.context.consume_members(1)?;
        self.context
            .reserve_vec(self.nodes, 1, "TypeTree node array")?;
        let node = TypeTreeNodeSeed {
            context: self.context,
            depth,
        }
        .deserialize(deserializer)?;
        self.nodes.values.push(node);
        Ok(())
    }
}

const TYPE_TREE_NODE_FIELDS: &[&str] = &[
    "type_name",
    "name",
    "byte_size",
    "variable_count",
    "index",
    "type_flags",
    "version",
    "meta_flags",
    "level",
    "type_str_offset",
    "name_str_offset",
    "ref_type_hash",
    "children",
];

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum TypeTreeNodeField {
    TypeName,
    Name,
    ByteSize,
    VariableCount,
    Index,
    TypeFlags,
    Version,
    MetaFlags,
    Level,
    TypeStrOffset,
    NameStrOffset,
    RefTypeHash,
    Children,
}

struct TypeTreeNodeSeed<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
    depth: u32,
}

impl<'de> DeserializeSeed<'de> for TypeTreeNodeSeed<'_, '_> {
    type Value = TypeTreeNode;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_struct(
            "TypeTreeNode",
            TYPE_TREE_NODE_FIELDS,
            TypeTreeNodeVisitor {
                context: self.context,
                depth: self.depth,
            },
        )
    }
}

struct TypeTreeNodeVisitor<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
    depth: u32,
}

impl<'de> Visitor<'de> for TypeTreeNodeVisitor<'_, '_> {
    type Value = TypeTreeNode;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a strict TypeTree node object")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut type_name = None;
        let mut name = None;
        let mut byte_size = None;
        let mut variable_count = None;
        let mut index = None;
        let mut type_flags = None;
        let mut version = None;
        let mut meta_flags = None;
        let mut level = None;
        let mut type_str_offset = None;
        let mut name_str_offset = None;
        let mut ref_type_hash = None;
        let mut children = None;

        while let Some(field) = map.next_key::<TypeTreeNodeField>()? {
            match field {
                TypeTreeNodeField::TypeName => {
                    if type_name.is_some() {
                        return Err(de::Error::duplicate_field("type_name"));
                    }
                    type_name = Some(map.next_value_seed(BudgetedStringSeed {
                        context: &mut *self.context,
                        label: "TypeTree node type name",
                    })?);
                }
                TypeTreeNodeField::Name => {
                    if name.is_some() {
                        return Err(de::Error::duplicate_field("name"));
                    }
                    name = Some(map.next_value_seed(BudgetedStringSeed {
                        context: &mut *self.context,
                        label: "TypeTree node name",
                    })?);
                }
                TypeTreeNodeField::ByteSize => {
                    set_once(&mut byte_size, map.next_value()?, "byte_size")?
                }
                TypeTreeNodeField::VariableCount => {
                    set_once(&mut variable_count, map.next_value()?, "variable_count")?
                }
                TypeTreeNodeField::Index => set_once(&mut index, map.next_value()?, "index")?,
                TypeTreeNodeField::TypeFlags => {
                    set_once(&mut type_flags, map.next_value()?, "type_flags")?
                }
                TypeTreeNodeField::Version => set_once(&mut version, map.next_value()?, "version")?,
                TypeTreeNodeField::MetaFlags => {
                    set_once(&mut meta_flags, map.next_value()?, "meta_flags")?
                }
                TypeTreeNodeField::Level => set_once(&mut level, map.next_value()?, "level")?,
                TypeTreeNodeField::TypeStrOffset => {
                    set_once(&mut type_str_offset, map.next_value()?, "type_str_offset")?
                }
                TypeTreeNodeField::NameStrOffset => {
                    set_once(&mut name_str_offset, map.next_value()?, "name_str_offset")?
                }
                TypeTreeNodeField::RefTypeHash => {
                    set_once(&mut ref_type_hash, map.next_value()?, "ref_type_hash")?
                }
                TypeTreeNodeField::Children => {
                    if children.is_some() {
                        return Err(de::Error::duplicate_field("children"));
                    }
                    children = Some(map.next_value_seed(TypeTreeNodesSeed {
                        context: &mut *self.context,
                        placement: NodePlacement::ChildOf(self.depth),
                    })?);
                }
            }
        }

        Ok(TypeTreeNode {
            type_name: required(type_name, "type_name")?,
            name: required(name, "name")?,
            byte_size: required(byte_size, "byte_size")?,
            variable_count: required(variable_count, "variable_count")?,
            index: required(index, "index")?,
            type_flags: required(type_flags, "type_flags")?,
            version: required(version, "version")?,
            meta_flags: required(meta_flags, "meta_flags")?,
            level: required(level, "level")?,
            type_str_offset: required(type_str_offset, "type_str_offset")?,
            name_str_offset: required(name_str_offset, "name_str_offset")?,
            ref_type_hash: required(ref_type_hash, "ref_type_hash")?,
            children: required(children, "children")?.into_vec(),
        })
    }
}

fn set_once<T, E: de::Error>(
    slot: &mut Option<T>,
    value: T,
    field: &'static str,
) -> std::result::Result<(), E> {
    if slot.replace(value).is_some() {
        return Err(E::duplicate_field(field));
    }
    Ok(())
}

fn required<T, E: de::Error>(value: Option<T>, field: &'static str) -> std::result::Result<T, E> {
    value.ok_or_else(|| E::missing_field(field))
}

struct ByteBufferSeed<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
}

impl<'de> DeserializeSeed<'de> for ByteBufferSeed<'_, '_> {
    type Value = BudgetedVec<u8>;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(ByteBufferVisitor {
            context: self.context,
        })
    }
}

struct ByteBufferVisitor<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
}

impl<'de> Visitor<'de> for ByteBufferVisitor<'_, '_> {
    type Value = BudgetedVec<u8>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of bytes")
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut bytes = BudgetedVec::default();
        while sequence
            .next_element_seed(ByteElementSeed {
                context: &mut *self.context,
                bytes: &mut bytes,
            })?
            .is_some()
        {}
        Ok(bytes)
    }
}

struct ByteElementSeed<'a, 'budget> {
    context: &'a mut JsonLoadContext<'budget>,
    bytes: &'a mut BudgetedVec<u8>,
}

impl<'de> DeserializeSeed<'de> for ByteElementSeed<'_, '_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.context.consume_members(1)?;
        self.context
            .reserve_vec(self.bytes, 1, "TypeTree string buffer")?;
        self.bytes.values.push(u8::deserialize(deserializer)?);
        Ok(())
    }
}

impl TypeTreeRegistry for JsonTypeTreeRegistry {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        self.inner.resolve(unity_version, class_id)
    }

    fn resolve_script(
        &self,
        unity_version: &str,
        class_id: i32,
        script_id: [u8; 16],
    ) -> Option<Arc<TypeTree>> {
        self.inner
            .resolve_script(unity_version, class_id, script_id)
    }
}

fn parse_hex_32_bytes(raw: &str) -> Option<[u8; 16]> {
    let s = raw.trim();
    if s.len() != 32 {
        return None;
    }

    let mut out = [0u8; 16];
    for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
        out[i] = (hex_nibble(chunk[0])? << 4) | hex_nibble(chunk[1])?;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEEP_REGISTRY_JSON: &str = r#"{
        "schema": 1,
        "entries": [{
            "unity_version": "2020.3.*",
            "class_id": 28,
            "type_tree": {
                "nodes": [{
                    "type_name": "Root", "name": "root", "byte_size": -1,
                    "variable_count": 0, "index": 0, "type_flags": 0, "version": 1,
                    "meta_flags": 0, "level": 0, "type_str_offset": 0,
                    "name_str_offset": 0, "ref_type_hash": 0,
                    "children": [{
                        "type_name": "Node", "name": "child", "byte_size": -1,
                        "variable_count": 0, "index": 1, "type_flags": 0, "version": 1,
                        "meta_flags": 0, "level": 1, "type_str_offset": 0,
                        "name_str_offset": 0, "ref_type_hash": 0,
                        "children": [{
                            "type_name": "int", "name": "leaf", "byte_size": 4,
                            "variable_count": 0, "index": 2, "type_flags": 0, "version": 1,
                            "meta_flags": 0, "level": 2, "type_str_offset": 0,
                            "name_str_offset": 0, "ref_type_hash": 0, "children": []
                        }]
                    }]
                }],
                "string_buffer": [65, 0],
                "version": 1, "platform": 1, "has_type_dependencies": false
            }
        }]
    }"#;

    fn single_node_registry_json(type_name: &str, name: &str, string_buffer: &str) -> String {
        format!(
            r#"{{"schema":1,"entries":[{{"class_id":28,"type_tree":{{"nodes":[{{"type_name":"{type_name}","name":"{name}","byte_size":4,"variable_count":0,"index":0,"type_flags":0,"version":1,"meta_flags":0,"level":0,"type_str_offset":0,"name_str_offset":0,"ref_type_hash":0,"children":[]}}],"string_buffer":[{string_buffer}],"version":1,"platform":1,"has_type_dependencies":false}}}}]}}"#
        )
    }

    fn wide_registry_json(node_count: usize, string_buffer_len: usize) -> String {
        let nodes = (0..node_count)
            .map(|index| {
                format!(
                    r#"{{"type_name":"int","name":"field{index}","byte_size":4,"variable_count":0,"index":{index},"type_flags":0,"version":1,"meta_flags":0,"level":0,"type_str_offset":0,"name_str_offset":0,"ref_type_hash":0,"children":[]}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let string_buffer = (0..string_buffer_len)
            .map(|value| (value % 256).to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"schema":1,"entries":[{{"class_id":28,"type_tree":{{"nodes":[{nodes}],"string_buffer":[{string_buffer}],"version":1,"platform":1,"has_type_dependencies":false}}}}]}}"#
        )
    }

    fn empty_tree_registry_entry_json(
        class_id: i32,
        unity_version: Option<&str>,
        script_id: Option<&str>,
        tag: u32,
    ) -> String {
        let unity_version = unity_version
            .map(|value| format!(r#""unity_version":"{value}","#))
            .unwrap_or_default();
        let script_id = script_id
            .map(|value| format!(r#","script_id":"{value}""#))
            .unwrap_or_default();
        format!(
            r#"{{{unity_version}"class_id":{class_id}{script_id},"type_tree":{{"nodes":[],"string_buffer":[],"version":{tag},"platform":{tag},"has_type_dependencies":false}}}}"#
        )
    }

    struct FragmentedReader<'a> {
        remaining: &'a [u8],
        max_chunk: usize,
    }

    impl<'a> FragmentedReader<'a> {
        fn new(remaining: &'a [u8], max_chunk: usize) -> Self {
            Self {
                remaining,
                max_chunk,
            }
        }
    }

    impl std::io::Read for FragmentedReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let count = self.remaining.len().min(buffer.len()).min(self.max_chunk);
            buffer[..count].copy_from_slice(&self.remaining[..count]);
            self.remaining = &self.remaining[count..];
            Ok(count)
        }
    }

    fn linear_registry_json(max_depth: u32) -> String {
        let mut node = format!(
            r#"{{"type_name":"int","name":"leaf","byte_size":4,"variable_count":0,"index":{max_depth},"type_flags":0,"version":1,"meta_flags":0,"level":{max_depth},"type_str_offset":0,"name_str_offset":0,"ref_type_hash":0,"children":[]}}"#
        );
        for depth in (0..max_depth).rev() {
            node = format!(
                r#"{{"type_name":"Node","name":"child","byte_size":-1,"variable_count":0,"index":{depth},"type_flags":0,"version":1,"meta_flags":0,"level":{depth},"type_str_offset":0,"name_str_offset":0,"ref_type_hash":0,"children":[{node}]}}"#
            );
        }
        format!(
            r#"{{"schema":1,"entries":[{{"class_id":28,"type_tree":{{"nodes":[{node}],"string_buffer":[],"version":1,"platform":1,"has_type_dependencies":false}}}}]}}"#
        )
    }

    fn dummy_tree(tag: u32) -> TypeTree {
        let mut t = TypeTree::new();
        t.version = tag;
        t.platform = tag;
        t
    }

    fn fixed_json_parser_budget(encoded_len: usize) -> u64 {
        let input_capacity = if encoded_len == 0 {
            0
        } else {
            encoded_len.checked_next_power_of_two().unwrap()
        };
        u64::try_from(encoded_len)
            .unwrap()
            .checked_mul(JSON_PARSER_WORK_MULTIPLIER)
            .unwrap()
            .checked_add(u64::try_from(input_capacity).unwrap())
            .unwrap()
            .checked_add(u64::try_from(JSON_ERROR_DIAGNOSTIC_BYTES).unwrap())
            .unwrap()
    }

    fn empty_registry_json(version: u32) -> String {
        format!(
            r#"{{"schema":1,"entries":[{{"class_id":28,"type_tree":{{"nodes":[],"string_buffer":[],"version":{version},"platform":{version},"has_type_dependencies":false}}}}]}}"#
        )
    }

    #[test]
    fn in_memory_registry_version_precedence() {
        let class_id = 28;

        let mut reg = InMemoryTypeTreeRegistry::default();
        reg.insert_any(class_id, dummy_tree(1));
        reg.insert_prefix("2020.3.".to_string(), class_id, dummy_tree(2));
        reg.insert_exact("2020.3.48f1".to_string(), class_id, dummy_tree(3));

        let exact = reg.resolve("2020.3.48f1", class_id).unwrap();
        assert_eq!(exact.version, 3);

        let prefix = reg.resolve("2020.3.9f1", class_id).unwrap();
        assert_eq!(prefix.version, 2);

        let any = reg.resolve("2019.4.40f1", class_id).unwrap();
        assert_eq!(any.version, 1);
    }

    #[test]
    fn in_memory_registry_longest_prefix_wins() {
        let class_id = 28;

        let mut reg = InMemoryTypeTreeRegistry::default();
        reg.insert_prefix("2020.".to_string(), class_id, dummy_tree(1));
        reg.insert_prefix("2020.3.".to_string(), class_id, dummy_tree(2));

        let t = reg.resolve("2020.3.48f1", class_id).unwrap();
        assert_eq!(t.version, 2);
    }

    #[test]
    fn composite_registry_first_match_wins() {
        let class_id = 28;

        let mut a = InMemoryTypeTreeRegistry::default();
        a.insert_any(class_id, dummy_tree(1));
        let mut b = InMemoryTypeTreeRegistry::default();
        b.insert_any(class_id, dummy_tree(2));

        let composite_ab = CompositeTypeTreeRegistry::new(vec![Arc::new(a), Arc::new(b)]);
        let t = composite_ab.resolve("2020.3.48f1", class_id).unwrap();
        assert_eq!(t.version, 1);

        let mut a2 = InMemoryTypeTreeRegistry::default();
        a2.insert_any(class_id, dummy_tree(1));
        let mut b2 = InMemoryTypeTreeRegistry::default();
        b2.insert_any(class_id, dummy_tree(2));

        let composite_ba = CompositeTypeTreeRegistry::new(vec![Arc::new(b2), Arc::new(a2)]);
        let t = composite_ba.resolve("2020.3.48f1", class_id).unwrap();
        assert_eq!(t.version, 2);
    }

    #[test]
    fn registry_path_factory_handles_empty_and_case_insensitive_tpk_paths() {
        let empty: [&Path; 0] = [];
        let mut empty_budget = AssetLoadBudget::default();
        assert!(
            CompositeTypeTreeRegistry::from_paths(&empty, &mut empty_budget)
                .unwrap()
                .is_none()
        );
        assert_eq!(empty_budget.usage(), Default::default());

        let temp = tempfile::tempdir().unwrap();
        let tpk = super::super::tpk::tests::build_minimal_tpk();
        for file_name in ["registry.tpk", "registry.TPK"] {
            let path = temp.path().join(file_name);
            std::fs::write(&path, &tpk).unwrap();
            let registry = CompositeTypeTreeRegistry::from_paths(
                std::slice::from_ref(&path),
                &mut AssetLoadBudget::default(),
            )
            .unwrap()
            .unwrap();
            assert!(registry.resolve("2020.3.0f1", 28).is_some());
        }
    }

    #[test]
    fn mixed_json_and_tpk_paths_preserve_first_match_priority() {
        let temp = tempfile::tempdir().unwrap();
        let json_path = temp.path().join("registry.json");
        let tpk_path = temp.path().join("registry.TPK");
        std::fs::write(&json_path, empty_registry_json(77)).unwrap();
        std::fs::write(&tpk_path, super::super::tpk::tests::build_minimal_tpk()).unwrap();

        let json_first = CompositeTypeTreeRegistry::from_paths(
            &[json_path.clone(), tpk_path.clone()],
            &mut AssetLoadBudget::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(json_first.resolve("2020.3.0f1", 28).unwrap().version, 77);

        let tpk_first = CompositeTypeTreeRegistry::from_paths(
            &[tpk_path, json_path],
            &mut AssetLoadBudget::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(tpk_first.resolve("2020.3.0f1", 28).unwrap().version, 0);
    }

    #[test]
    fn json_registry_supports_wildcard_and_exact() {
        let json = r#"
        {
          "schema": 1,
          "entries": [
            { "unity_version": "2020.3.*", "class_id": 28, "type_tree": { "nodes": [], "string_buffer": [], "version": 2, "platform": 2, "has_type_dependencies": false } },
            { "unity_version": "2020.3.48f1", "class_id": 28, "type_tree": { "nodes": [], "string_buffer": [], "version": 3, "platform": 3, "has_type_dependencies": false } },
            { "class_id": 28, "type_tree": { "nodes": [], "string_buffer": [], "version": 1, "platform": 1, "has_type_dependencies": false } }
          ]
        }
        "#;

        let reg = JsonTypeTreeRegistry::from_reader(
            json.as_bytes(),
            &mut unity_asset_core::AssetLoadBudget::default(),
        )
        .unwrap();

        let exact = reg.resolve("2020.3.48f1", 28).unwrap();
        assert_eq!(exact.version, 3);

        let prefix = reg.resolve("2020.3.9f1", 28).unwrap();
        assert_eq!(prefix.version, 2);

        let any = reg.resolve("2019.4.40f1", 28).unwrap();
        assert_eq!(any.version, 1);
    }

    #[test]
    fn json_registry_preserves_first_occurrence_within_class_and_script_buckets() {
        const SCRIPT_EXACT: &str = "01010101010101010101010101010101";
        const SCRIPT_PREFIX: &str = "02020202020202020202020202020202";
        const SCRIPT_ANY: &str = "03030303030303030303030303030303";

        let mut entries = Vec::new();
        for (class_id, version, script_id, first, second) in [
            (101, Some("2020.3.48f1"), None, 1, 2),
            (102, Some("2020.3.*"), None, 3, 4),
            (103, None, None, 5, 6),
            (114, Some("2020.3.48f1"), Some(SCRIPT_EXACT), 7, 8),
            (114, Some("2020.3.*"), Some(SCRIPT_PREFIX), 9, 10),
            (114, None, Some(SCRIPT_ANY), 11, 12),
        ] {
            entries.push(empty_tree_registry_entry_json(
                class_id, version, script_id, first,
            ));
            entries.push(empty_tree_registry_entry_json(
                class_id, version, script_id, second,
            ));
        }
        let json = format!(r#"{{"schema":2,"entries":[{}]}}"#, entries.join(","));
        let registry =
            JsonTypeTreeRegistry::from_reader(json.as_bytes(), &mut AssetLoadBudget::default())
                .unwrap();

        assert_eq!(registry.resolve("2020.3.48f1", 101).unwrap().version, 1);
        assert_eq!(registry.resolve("2020.3.9f1", 102).unwrap().version, 3);
        assert_eq!(registry.resolve("2019.4.40f1", 103).unwrap().version, 5);
        assert_eq!(
            registry
                .resolve_script("2020.3.48f1", 114, [0x01; 16])
                .unwrap()
                .version,
            7
        );
        assert_eq!(
            registry
                .resolve_script("2020.3.9f1", 114, [0x02; 16])
                .unwrap()
                .version,
            9
        );
        assert_eq!(
            registry
                .resolve_script("2019.4.40f1", 114, [0x03; 16])
                .unwrap()
                .version,
            11
        );
    }

    #[test]
    fn json_registry_normalizes_descending_unique_keys_before_grouping() {
        const COUNT: i32 = 33;
        let entries = (0..COUNT)
            .rev()
            .map(|class_id| empty_tree_registry_entry_json(class_id, None, None, class_id as u32))
            .collect::<Vec<_>>();
        let json = format!(r#"{{"schema":1,"entries":[{}]}}"#, entries.join(","));
        let registry =
            JsonTypeTreeRegistry::from_reader(json.as_bytes(), &mut AssetLoadBudget::default())
                .unwrap();

        assert_eq!(registry.inner.by_class_id.buckets.accounted_capacity, 64);
        assert!(
            registry
                .inner
                .by_class_id
                .buckets
                .values
                .windows(2)
                .all(|buckets| buckets[0].key < buckets[1].key)
        );
        for class_id in 0..COUNT {
            assert_eq!(
                registry.resolve("2020.3", class_id).unwrap().version,
                class_id as u32
            );
        }
    }

    #[test]
    fn in_memory_registry_resolves_script_id() {
        let script_id = [0x01u8; 16];

        let mut reg = InMemoryTypeTreeRegistry::default();
        reg.insert_script_any(script_id, dummy_tree(1));

        let t = reg
            .resolve_script(
                "2020.3.48f1",
                unity_asset_core::class_ids::MONO_BEHAVIOUR,
                script_id,
            )
            .unwrap();
        assert_eq!(t.version, 1);
    }

    #[test]
    fn json_registry_schema_2_supports_script_id_hex() {
        let json = r#"
        {
          "schema": 2,
          "entries": [
            { "unity_version": "2020.3.*", "class_id": 114, "script_id": "01010101010101010101010101010101", "type_tree": { "nodes": [], "string_buffer": [], "version": 2, "platform": 2, "has_type_dependencies": false } }
          ]
        }
        "#;

        let reg = JsonTypeTreeRegistry::from_reader(
            json.as_bytes(),
            &mut unity_asset_core::AssetLoadBudget::default(),
        )
        .unwrap();
        let script_id = [0x01u8; 16];
        let t = reg.resolve_script("2020.3.9f1", 114, script_id).unwrap();
        assert_eq!(t.version, 2);
    }

    #[test]
    fn budgeted_vec_grows_geometrically_and_preflights_capacity() {
        use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};

        let slot_bytes = u64::try_from(size_of::<u32>()).unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 8 * slot_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let mut values = BudgetedVec::default();

        for value in 0_u32..5 {
            values
                .reserve_budgeted(1, &mut budget, "test registry vector")
                .unwrap();
            values.values.push(value);
        }

        assert_eq!(values.values, [0, 1, 2, 3, 4]);
        assert_eq!(values.accounted_capacity, 8);
        assert!(values.values.capacity() >= values.accounted_capacity);
        assert_eq!(budget.usage().bytes, 8 * slot_bytes);

        let usage_before = budget.usage();
        let capacity_before = values.values.capacity();
        let error = values
            .reserve_budgeted(4, &mut budget, "test registry vector")
            .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == 8 * slot_bytes && requested == 16 * slot_bytes
        ));
        assert_eq!(values.accounted_capacity, 8);
        assert_eq!(values.values.capacity(), capacity_before);
        assert_eq!(budget.usage(), usage_before);
    }

    #[test]
    fn json_registry_budget_is_independent_of_reader_fragmentation() {
        let json = wide_registry_json(17, 17);
        let mut contiguous_budget = AssetLoadBudget::default();
        let contiguous =
            JsonTypeTreeRegistry::from_reader(json.as_bytes(), &mut contiguous_budget).unwrap();

        let mut fragmented_budget = AssetLoadBudget::default();
        let fragmented = JsonTypeTreeRegistry::from_reader(
            FragmentedReader::new(json.as_bytes(), 1),
            &mut fragmented_budget,
        )
        .unwrap();

        assert_eq!(fragmented_budget.usage(), contiguous_budget.usage());
        for registry in [&contiguous, &fragmented] {
            let tree = registry.resolve("2020.3", 28).unwrap();
            assert_eq!(tree.nodes.len(), 17);
            assert_eq!(tree.string_buffer.len(), 17);
            assert!(tree.nodes.capacity() >= 32);
            assert!(tree.string_buffer.capacity() >= 32);
            assert_eq!(registry.inner.by_class_id.buckets.accounted_capacity, 1);
            assert_eq!(
                registry.inner.by_class_id.buckets.values[0]
                    .entries
                    .accounted_capacity,
                1
            );
        }
    }

    #[test]
    fn json_registry_input_hard_cap_is_checked_before_retention_and_parser_work() {
        let mut exact_budget = AssetLoadBudget::default();
        let exact =
            read_registry_json_input(&mut FragmentedReader::new(b"1234", 1), &mut exact_budget, 4)
                .unwrap();
        assert_eq!(exact.as_slice(), b"1234");

        let mut over_budget = AssetLoadBudget::default();
        let error =
            read_registry_json_input(&mut FragmentedReader::new(b"12345", 1), &mut over_budget, 4)
                .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::ResourceLimitExceeded(message)
                if message.contains("requested 5 bytes, limit 4")
        ));
        assert_eq!(
            over_budget.usage(),
            exact_budget.usage(),
            "the rejected byte must not consume parser or retained-input budget"
        );
    }

    #[test]
    fn json_registry_wide_collections_charge_geometric_capacity() {
        const COUNT: usize = 5;
        const CAPACITY: usize = 8;

        let json = wide_registry_json(COUNT, COUNT);
        let retained_strings = (0..COUNT)
            .map(|index| "int".len() + format!("field{index}").len())
            .sum::<usize>();
        let fixed_storage = CAPACITY * size_of::<TypeTreeNode>()
            + CAPACITY * size_of::<u8>()
            + size_of::<PendingRegistryEntry>()
            + size_of::<RegistryBucket<i32>>()
            + size_of::<RegistryEntry>()
            + size_of::<ArcAllocation<TypeTree>>()
            + retained_strings;
        let expected = fixed_json_parser_budget(json.len()) + u64::try_from(fixed_storage).unwrap();

        let mut budget = AssetLoadBudget::default();
        let registry = JsonTypeTreeRegistry::from_reader(json.as_bytes(), &mut budget).unwrap();
        assert_eq!(budget.usage().bytes, expected);

        let tree = registry.resolve("2020.3", 28).unwrap();
        assert_eq!(tree.nodes.len(), COUNT);
        assert_eq!(tree.string_buffer.len(), COUNT);
        assert!(tree.nodes.capacity() >= CAPACITY);
        assert!(tree.string_buffer.capacity() >= CAPACITY);
    }

    #[test]
    fn json_registry_encoded_input_obeys_exact_byte_budget() {
        use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};

        let json = br#"{"schema":1,"entries":[]}"#;
        let exact_limit = fixed_json_parser_budget(json.len());
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_limit,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        JsonTypeTreeRegistry::from_reader(json.as_slice(), &mut exact).unwrap();
        assert_eq!(exact.usage().bytes, exact_limit);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_limit - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = JsonTypeTreeRegistry::from_reader(json.as_slice(), &mut one_short).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == exact_limit - 1 && requested == exact_limit
        ));
    }

    #[test]
    fn json_registry_parser_work_is_charged_before_escaped_string_decode() {
        use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};

        let json = br#"{"sch\u0065ma":1,"entries":[]}"#;
        let exact_limit = fixed_json_parser_budget(json.len());

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_limit,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        JsonTypeTreeRegistry::from_reader(json.as_slice(), &mut exact).unwrap();
        assert_eq!(exact.usage().bytes, exact_limit);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_limit - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = JsonTypeTreeRegistry::from_reader(json.as_slice(), &mut one_short).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == exact_limit - 1 && requested == exact_limit
        ));
    }

    #[test]
    fn json_registry_semantic_depth_budget_owns_deep_nesting_boundary() {
        use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};

        const MAX_DEPTH: u32 = MAX_JSON_TYPE_TREE_DEPTH;
        let json = linear_registry_json(MAX_DEPTH);
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: MAX_DEPTH,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        JsonTypeTreeRegistry::from_reader(json.as_bytes(), &mut exact).unwrap();
        assert_eq!(exact.usage().max_observed_depth, MAX_DEPTH);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: MAX_DEPTH - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = JsonTypeTreeRegistry::from_reader(json.as_bytes(), &mut one_short).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "depth",
                limit,
                requested,
            }) if limit == u64::from(MAX_DEPTH - 1) && requested == u64::from(MAX_DEPTH)
        ));
    }

    #[test]
    fn json_registry_has_a_typed_depth_limit_before_serde_recursion_limit() {
        use unity_asset_core::{AssetLoadBudget, BudgetError};

        let requested = MAX_JSON_TYPE_TREE_DEPTH + 1;
        let json = linear_registry_json(requested);
        let error =
            JsonTypeTreeRegistry::from_reader(json.as_bytes(), &mut AssetLoadBudget::default())
                .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "json_typetree_depth",
                limit,
                requested: actual,
            }) if limit == u64::from(MAX_JSON_TYPE_TREE_DEPTH)
                && actual == u64::from(requested)
        ));
    }

    #[test]
    fn json_registry_rejects_huge_capacity_before_allocation() {
        use unity_asset_core::{AssetLoadBudget, BudgetError};

        let mut values = BudgetedVec::<TypeTreeNode>::default();
        let mut budget = AssetLoadBudget::default();
        let error = values
            .reserve_budgeted(usize::MAX, &mut budget, "TypeTree node array")
            .unwrap_err();

        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::ArithmeticOverflow { resource: "bytes" })
        ));
        assert_eq!(values.values.capacity(), 0);
        assert_eq!(values.accounted_capacity, 0);
        assert_eq!(budget.usage().bytes, 0);
    }

    #[test]
    fn json_registry_rejects_unknown_and_duplicate_fields_at_every_object_level() {
        let invalid_documents = [
            r#"{"schema":1,"entries":[],"unknown":true}"#,
            r#"{"schema":1,"schema":1,"entries":[]}"#,
            r#"{"schema":1,"entries":[{"class_id":28,"type_tree":{"nodes":[],"string_buffer":[],"version":1,"platform":1,"has_type_dependencies":false},"unknown":true}]}"#,
            r#"{"schema":1,"entries":[{"class_id":28,"class_id":28,"type_tree":{"nodes":[],"string_buffer":[],"version":1,"platform":1,"has_type_dependencies":false}}]}"#,
            r#"{"schema":1,"entries":[{"class_id":28,"type_tree":{"nodes":[],"string_buffer":[],"version":1,"platform":1,"has_type_dependencies":false,"unknown":true}}]}"#,
            r#"{"schema":1,"entries":[{"class_id":28,"type_tree":{"nodes":[],"string_buffer":[],"version":1,"version":1,"platform":1,"has_type_dependencies":false}}]}"#,
            r#"{"schema":1,"entries":[{"class_id":28,"type_tree":{"nodes":[{"type_name":"int","name":"value","byte_size":4,"variable_count":0,"index":0,"type_flags":0,"version":1,"meta_flags":0,"level":0,"type_str_offset":0,"name_str_offset":0,"ref_type_hash":0,"children":[],"unknown":true}],"string_buffer":[],"version":1,"platform":1,"has_type_dependencies":false}}]}"#,
            r#"{"schema":1,"entries":[{"class_id":28,"type_tree":{"nodes":[{"type_name":"int","name":"value","name":"value","byte_size":4,"variable_count":0,"index":0,"type_flags":0,"version":1,"meta_flags":0,"level":0,"type_str_offset":0,"name_str_offset":0,"ref_type_hash":0,"children":[]}],"string_buffer":[],"version":1,"platform":1,"has_type_dependencies":false}}]}"#,
        ];

        for json in invalid_documents {
            let error = JsonTypeTreeRegistry::from_reader(
                json.as_bytes(),
                &mut unity_asset_core::AssetLoadBudget::default(),
            )
            .unwrap_err();
            assert!(
                matches!(error, BinaryError::InvalidData(_)),
                "{json}: {error}"
            );
        }
    }

    #[test]
    fn json_registry_entries_members_and_depth_obey_exact_limits() {
        use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};

        let mut probe = AssetLoadBudget::default();
        JsonTypeTreeRegistry::from_reader(DEEP_REGISTRY_JSON.as_bytes(), &mut probe).unwrap();
        let usage = probe.usage();
        assert_eq!(usage.entries, 4);
        assert_eq!(usage.members, 6);
        assert_eq!(usage.max_observed_depth, 2);

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: usage.entries,
            max_members: usage.members,
            max_depth: usage.max_observed_depth,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        JsonTypeTreeRegistry::from_reader(DEEP_REGISTRY_JSON.as_bytes(), &mut exact).unwrap();
        assert_eq!(exact.usage().entries, usage.entries);
        assert_eq!(exact.usage().members, usage.members);
        assert_eq!(exact.usage().max_observed_depth, usage.max_observed_depth);

        let mut entries_short = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: usage.entries - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error =
            JsonTypeTreeRegistry::from_reader(DEEP_REGISTRY_JSON.as_bytes(), &mut entries_short)
                .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "entries",
                limit,
                requested,
            }) if limit == usage.entries - 1 && requested == usage.entries
        ));
        assert_eq!(entries_short.usage().entries, usage.entries - 1);

        let mut members_short = AssetLoadBudget::new(AssetLoadLimits {
            max_members: usage.members - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error =
            JsonTypeTreeRegistry::from_reader(DEEP_REGISTRY_JSON.as_bytes(), &mut members_short)
                .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "members",
                limit,
                requested,
            }) if limit == usage.members - 1 && requested == usage.members
        ));
        assert_eq!(members_short.usage().members, usage.members - 1);

        let mut depth_short = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: usage.max_observed_depth - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error =
            JsonTypeTreeRegistry::from_reader(DEEP_REGISTRY_JSON.as_bytes(), &mut depth_short)
                .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "depth",
                limit: 1,
                requested: 2,
            })
        ));
        assert_eq!(depth_short.usage().max_observed_depth, 1);
    }

    #[test]
    fn json_registry_retained_graph_obeys_exact_byte_budget() {
        use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};

        let empty = single_node_registry_json("", "", "");
        let populated = single_node_registry_json("Root", "field", "1,2,3");
        let mut empty_budget = AssetLoadBudget::default();
        JsonTypeTreeRegistry::from_reader(empty.as_bytes(), &mut empty_budget).unwrap();
        let mut probe = AssetLoadBudget::default();
        JsonTypeTreeRegistry::from_reader(populated.as_bytes(), &mut probe).unwrap();
        let retained_empty = empty_budget.usage().bytes - fixed_json_parser_budget(empty.len());
        let retained_populated = probe.usage().bytes - fixed_json_parser_budget(populated.len());
        assert_eq!(retained_populated - retained_empty, 4 + 5 + 4);

        let exact_limit = probe.usage().bytes;
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_limit,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let registry = JsonTypeTreeRegistry::from_reader(populated.as_bytes(), &mut exact).unwrap();
        assert_eq!(exact.usage().bytes, exact_limit);
        let before_resolve = exact.usage();
        assert!(registry.resolve("2020.3", 28).is_some());
        assert_eq!(exact.usage(), before_resolve);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_limit - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error =
            JsonTypeTreeRegistry::from_reader(populated.as_bytes(), &mut one_short).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == exact_limit - 1 && requested == exact_limit
        ));
        assert!(one_short.usage().bytes < exact_limit);
    }

    #[test]
    fn json_registry_fixed_graph_storage_matches_the_declared_budget_model() {
        let json = single_node_registry_json("Root", "field", "1,2,3");
        let class_slot = size_of::<RegistryBucket<i32>>();
        let fixed_storage = size_of::<TypeTreeNode>()
            + 4 * size_of::<u8>()
            + size_of::<PendingRegistryEntry>()
            + class_slot
            + size_of::<RegistryEntry>()
            + size_of::<ArcAllocation<TypeTree>>();
        let retained_strings = "Root".len() + "field".len();
        let expected = fixed_json_parser_budget(json.len())
            + u64::try_from(fixed_storage + retained_strings).unwrap();

        let mut budget = AssetLoadBudget::default();
        JsonTypeTreeRegistry::from_reader(json.as_bytes(), &mut budget).unwrap();
        assert_eq!(budget.usage().bytes, expected);
    }
}
