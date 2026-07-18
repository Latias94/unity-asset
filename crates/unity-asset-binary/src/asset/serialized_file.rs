//! Validated SerializedFile model and object-table access.

use super::format::{SerializedFileFormat, SerializedFileRegions};
use super::header::SerializedFileHeader;
use super::types::{
    FileIdentifier, LocalSerializedObjectIdentifier, ObjectInfo, SerializedType, TypeRegistry,
};
use super::validation;
use crate::data_view::DataView;
use crate::error::{BinaryError, Result};
use crate::shared_bytes::SharedBytes;
use crate::typetree::{ManagedReferenceCatalog, TypeTree, TypeTreeRegistry, TypeTreeSchema};
use once_cell::sync::OnceCell;
use std::collections::HashMap;
use std::mem::size_of;
use std::ops::Range;
use std::sync::{Arc, OnceLock};
use unity_asset_core::AssetLoadBudget;

/// Validated fields decoded from a SerializedFile image before backing storage is attached.
#[derive(Debug)]
pub(super) struct ParsedParts {
    pub(super) format: SerializedFileFormat,
    pub(super) regions: SerializedFileRegions,
    pub(super) header: SerializedFileHeader,
    pub(super) unity_version: String,
    pub(super) target_platform: i32,
    pub(super) enable_type_tree: bool,
    pub(super) types: Vec<SerializedType>,
    pub(super) legacy_big_id: Option<i32>,
    pub(super) objects: Vec<ObjectInfo>,
    pub(super) script_types: Vec<LocalSerializedObjectIdentifier>,
    pub(super) externals: Vec<FileIdentifier>,
    pub(super) ref_types: Vec<SerializedType>,
    pub(super) user_information: String,
}

/// Complete SerializedFile structure.
///
/// This structure represents a complete Unity SerializedFile with all its
/// metadata, type information, and object data.
#[derive(Debug)]
pub struct SerializedFile {
    /// Validated wire-format capabilities used to parse this file.
    format: SerializedFileFormat,
    /// Checked physical regions within the SerializedFile image.
    regions: SerializedFileRegions,
    /// File header.
    pub header: SerializedFileHeader,
    /// Unity version string.
    pub unity_version: String,
    /// Target platform.
    pub target_platform: i32,
    /// Whether type tree is enabled.
    enable_type_tree: bool,
    /// Optional external TypeTree registry for stripped files (best-effort).
    type_tree_registry: Option<Arc<dyn TypeTreeRegistry>>,
    /// Type information.
    types: Vec<SerializedType>,
    /// Exact legacy `bigIdEnabled` value when the format stores it.
    legacy_big_id: Option<i32>,
    /// Object information in immutable table order.
    objects: Vec<ObjectInfo>,
    /// Script types.
    pub script_types: Vec<LocalSerializedObjectIdentifier>,
    /// External file references.
    pub externals: Vec<FileIdentifier>,
    /// Reference types.
    ref_types: Vec<SerializedType>,
    /// User information.
    pub user_information: String,
    /// Raw file data.
    data: DataView,
    object_index_by_path_id: OnceLock<HashMap<i64, usize>>,
    schema_cache: SerializedFileSchemaCache,
}

#[derive(Debug, Default)]
struct SerializedFileSchemaCache {
    managed: SchemaCacheCell<Arc<ManagedReferenceCatalog>>,
    internal: SchemaCacheCell<Box<[SchemaCacheCell<TypeTreeSchema>]>>,
}

type SchemaCacheCell<T> = OnceCell<SchemaCacheOutcome<T>>;

#[derive(Debug)]
enum SchemaCacheOutcome<T> {
    Value(T),
    InvalidData(String),
}

impl<T> SchemaCacheOutcome<T> {
    fn as_result(&self) -> Result<&T> {
        match self {
            Self::Value(value) => Ok(value),
            Self::InvalidData(message) => Err(BinaryError::invalid_data(message.clone())),
        }
    }
}

fn get_or_try_cache<T>(
    cell: &SchemaCacheCell<T>,
    initialize: impl FnOnce() -> Result<T>,
) -> Result<&T> {
    cell.get_or_try_init(|| match initialize() {
        Ok(value) => Ok(SchemaCacheOutcome::Value(value)),
        Err(BinaryError::InvalidData(message)) => Ok(SchemaCacheOutcome::InvalidData(message)),
        Err(error) => Err(error),
    })?
    .as_result()
}

impl SerializedFile {
    pub(super) fn from_parsed_parts(parts: ParsedParts, view: DataView) -> Result<Self> {
        let file_range = range_to_usize(&parts.regions.file, "SerializedFile")?;
        let base_offset = view.base_offset();
        let absolute_start = base_offset
            .checked_add(file_range.start)
            .ok_or_else(|| BinaryError::invalid_data("SerializedFile base offset overflow"))?;
        let absolute_end = base_offset
            .checked_add(file_range.end)
            .ok_or_else(|| BinaryError::invalid_data("SerializedFile end offset overflow"))?;
        let data =
            DataView::from_shared_range(view.backing_shared(), absolute_start..absolute_end)?;

        Ok(Self {
            format: parts.format,
            regions: parts.regions,
            header: parts.header,
            unity_version: parts.unity_version,
            target_platform: parts.target_platform,
            enable_type_tree: parts.enable_type_tree,
            type_tree_registry: None,
            types: parts.types,
            legacy_big_id: parts.legacy_big_id,
            objects: parts.objects,
            script_types: parts.script_types,
            externals: parts.externals,
            ref_types: parts.ref_types,
            user_information: parts.user_information,
            data,
            object_index_by_path_id: OnceLock::new(),
            schema_cache: SerializedFileSchemaCache::default(),
        })
    }

    /// Returns object metadata in immutable SerializedFile table order.
    ///
    /// Object topology is intentionally not mutable after parsing because path-ID lookup is part
    /// of this snapshot's identity contract. Payload overrides use [`Self::find_object_mut`].
    pub fn objects(&self) -> &[ObjectInfo] {
        &self.objects
    }

    /// Returns whether embedded TypeTrees may be used for object schemas.
    pub const fn type_tree_enabled(&self) -> bool {
        self.enable_type_tree
    }

    /// Enables or disables embedded TypeTree lookup without mutating the retained type table.
    pub fn set_type_tree_enabled(&mut self, enabled: bool) {
        self.enable_type_tree = enabled;
    }

    /// Returns the validated SerializedType table in wire order.
    pub fn types(&self) -> &[SerializedType] {
        &self.types
    }

    /// Returns mutable type metadata after invalidating every derived schema.
    ///
    /// This exists for format construction and adversarial tests. Normal object consumers should
    /// treat a parsed SerializedFile as an immutable snapshot and use [`Self::types`].
    pub fn types_mut(&mut self) -> &mut Vec<SerializedType> {
        self.invalidate_schema_cache();
        &mut self.types
    }

    /// Returns the managed-reference type catalog in wire order.
    pub fn ref_types(&self) -> &[SerializedType] {
        &self.ref_types
    }

    /// Returns mutable managed-reference metadata after invalidating every derived schema.
    pub fn ref_types_mut(&mut self) -> &mut Vec<SerializedType> {
        self.invalidate_schema_cache();
        &mut self.ref_types
    }

    /// Returns the external registry used when an embedded TypeTree is unavailable.
    pub fn type_tree_registry(&self) -> Option<&Arc<dyn TypeTreeRegistry>> {
        self.type_tree_registry.as_ref()
    }

    /// Returns the validated format capability profile.
    pub const fn format(&self) -> SerializedFileFormat {
        self.format
    }

    /// Returns the checked physical regions of this file image.
    pub fn regions(&self) -> &SerializedFileRegions {
        &self.regions
    }

    /// Returns the exact legacy `bigIdEnabled` wire value when present.
    pub const fn legacy_big_id(&self) -> Option<i32> {
        self.legacy_big_id
    }

    /// Returns whether the legacy object table stores 64-bit path IDs.
    pub fn uses_big_ids(&self) -> bool {
        self.legacy_big_id.is_some_and(|value| value != 0)
    }

    pub fn set_type_tree_registry(&mut self, registry: Option<Arc<dyn TypeTreeRegistry>>) {
        self.type_tree_registry = registry;
    }

    fn invalidate_schema_cache(&mut self) {
        self.schema_cache = SerializedFileSchemaCache::default();
    }

    /// Get the raw file data.
    pub fn data(&self) -> &[u8] {
        self.data.as_bytes()
    }

    /// Get the backing shared buffer for this file's bytes.
    pub fn data_shared(&self) -> SharedBytes {
        self.data.backing_shared()
    }

    /// Base offset of this file within the backing storage returned by [`Self::data_shared`].
    pub fn data_base_offset(&self) -> usize {
        self.data.base_offset()
    }

    /// A stable identity key for caches: `(backing_ptr, base_offset, len)`.
    pub fn data_identity_key(&self) -> (usize, usize, usize) {
        self.data.identity_key()
    }

    /// Get the raw bytes for an object without requiring preloaded per-object buffers.
    pub fn object_bytes<'a>(&'a self, info: &ObjectInfo) -> Result<&'a [u8]> {
        let range = range_to_usize(&(info.byte_start()..info.byte_end()?), "object payload")?;
        let data = self.data();
        if range.end > data.len() {
            return Err(BinaryError::invalid_data(format!(
                "Object data out of bounds (path_id={}, start={}, size={}, file_len={})",
                info.path_id(),
                range.start,
                info.byte_size(),
                data.len()
            )));
        }
        Ok(&data[range])
    }

    /// Get object count.
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// Get type count.
    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    fn object_index(&self) -> &HashMap<i64, usize> {
        self.object_index_by_path_id.get_or_init(|| {
            let mut map = HashMap::with_capacity(self.objects.len());
            for (index, object) in self.objects.iter().enumerate() {
                map.insert(object.path_id(), index);
            }
            map
        })
    }

    /// Find object by path ID.
    pub fn find_object(&self, path_id: i64) -> Option<&ObjectInfo> {
        self.object_index()
            .get(&path_id)
            .and_then(|index| self.objects.get(*index))
    }

    /// Returns an object whose payload may be overridden without changing table identity.
    pub fn find_object_mut(&mut self, path_id: i64) -> Option<&mut ObjectInfo> {
        let index = self.object_index().get(&path_id).copied()?;
        self.objects.get_mut(index)
    }

    /// Find type by class ID.
    pub fn find_type(&self, class_id: i32) -> Option<&SerializedType> {
        self.types
            .iter()
            .find(|serialized_type| serialized_type.class_id == class_id)
    }

    /// Get all objects of a specific type.
    pub fn objects_of_type(&self, class_id: i32) -> Vec<&ObjectInfo> {
        self.objects
            .iter()
            .filter(|object| object.class_id() == class_id)
            .collect()
    }

    /// Create a type registry from this file.
    pub fn create_type_registry(&self) -> TypeRegistry {
        let mut registry = TypeRegistry::new();
        for serialized_type in &self.types {
            registry.add_type(serialized_type.clone());
        }
        registry
    }

    pub(crate) fn cached_internal_schema(
        &self,
        type_index: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<TypeTreeSchema> {
        let tree = &self
            .types
            .get(type_index)
            .ok_or_else(|| {
                BinaryError::invalid_data(format!(
                    "SerializedType index {type_index} has no schema cache cell"
                ))
            })?
            .type_tree;
        let cells = get_or_try_cache(&self.schema_cache.internal, || {
            let count = self.types.len();
            let allocation = count
                .checked_mul(size_of::<SchemaCacheCell<TypeTreeSchema>>())
                .ok_or_else(|| {
                    BinaryError::memory_error("SerializedFile schema cache size overflow")
                })?;
            let allocation = u64::try_from(allocation).map_err(|_| {
                BinaryError::memory_error("SerializedFile schema cache size does not fit u64")
            })?;
            budget.check_bytes(allocation)?;

            let mut cells = Vec::new();
            cells.try_reserve_exact(count).map_err(|error| {
                BinaryError::memory_error(format!(
                    "Failed to reserve {count} SerializedFile schema cache cells: {error}"
                ))
            })?;
            cells.resize_with(count, SchemaCacheCell::new);
            budget.consume_bytes(allocation)?;
            Ok(cells.into_boxed_slice())
        })?;
        let cell = cells.get(type_index).ok_or_else(|| {
            BinaryError::invalid_data(format!(
                "SerializedType index {type_index} has no schema cache cell"
            ))
        })?;
        get_or_try_cache(cell, || self.compile_schema(tree, budget)).cloned()
    }

    pub(crate) fn compile_schema(
        &self,
        tree: &TypeTree,
        budget: &mut AssetLoadBudget,
    ) -> Result<TypeTreeSchema> {
        TypeTreeSchema::compile_with_catalog(tree, budget, |budget| {
            get_or_try_cache(&self.schema_cache.managed, || {
                ManagedReferenceCatalog::compile(&self.ref_types, budget).map(Arc::new)
            })
            .cloned()
        })
    }

    /// Get file statistics.
    pub fn statistics(&self) -> FileStatistics {
        FileStatistics {
            version: self.header.version,
            unity_version: self.unity_version.clone(),
            file_size: self.header.file_size,
            object_count: self.objects.len(),
            type_count: self.types.len(),
            script_type_count: self.script_types.len(),
            external_count: self.externals.len(),
            has_type_tree: self.enable_type_tree,
            target_platform: self.target_platform,
        }
    }

    /// Validate the entire file's retained wire state.
    pub fn validate(&self) -> Result<()> {
        validation::validate_file(self)
    }

    pub(super) fn load_object_data(&mut self, budget: &mut AssetLoadBudget) -> Result<()> {
        let backing = self.data.backing_shared();
        let start = self.data.base_offset();
        let len = self.data.len();
        let end = start
            .checked_add(len)
            .ok_or_else(|| BinaryError::invalid_data("SerializedFile backing range overflow"))?;
        let bytes = backing.as_bytes().get(start..end).ok_or_else(|| {
            BinaryError::invalid_data("SerializedFile backing range is out of bounds")
        })?;
        let file_len = bytes.len();
        let mut payload_bytes = 0_u64;
        for object in &self.objects {
            let range =
                range_to_usize(&(object.byte_start()..object.byte_end()?), "object payload")?;
            if range.end > file_len {
                return Err(BinaryError::invalid_data(format!(
                    "Object data out of bounds (path_id={}, start={}, size={}, file_len={})",
                    object.path_id(),
                    range.start,
                    object.byte_size(),
                    file_len
                )));
            }
            payload_bytes = payload_bytes
                .checked_add(u64::from(object.byte_size()))
                .ok_or_else(|| BinaryError::invalid_data("Object payload byte total overflow"))?;
        }
        budget.consume_bytes(payload_bytes)?;

        for object in &mut self.objects {
            let range =
                range_to_usize(&(object.byte_start()..object.byte_end()?), "object payload")?;
            let mut payload = Vec::new();
            payload.try_reserve_exact(range.len()).map_err(|error| {
                BinaryError::memory_error(format!(
                    "Failed to reserve {} bytes for object {}: {error}",
                    range.len(),
                    object.path_id()
                ))
            })?;
            payload.extend_from_slice(&bytes[range]);
            object.set_data(payload);
        }
        Ok(())
    }
}

fn range_to_usize(range: &Range<u64>, label: &str) -> Result<Range<usize>> {
    if range.start > range.end {
        return Err(BinaryError::invalid_data(format!(
            "Invalid {label} range {}..{}",
            range.start, range.end
        )));
    }
    let start = usize::try_from(range.start).map_err(|_| {
        BinaryError::invalid_data(format!("{label} start {} does not fit usize", range.start))
    })?;
    let end = usize::try_from(range.end).map_err(|_| {
        BinaryError::invalid_data(format!("{label} end {} does not fit usize", range.end))
    })?;
    Ok(start..end)
}

/// File statistics.
#[derive(Debug, Clone)]
pub struct FileStatistics {
    pub version: u32,
    pub unity_version: String,
    pub file_size: u64,
    pub object_count: usize,
    pub type_count: usize,
    pub script_type_count: usize,
    pub external_count: usize,
    pub has_type_tree: bool,
    pub target_platform: i32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, mpsc};
    use std::thread;

    #[test]
    fn schema_cache_initializes_once_across_concurrent_callers() {
        const CALLERS: usize = 8;
        let cache = Arc::new(SchemaCacheCell::<usize>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(CALLERS));

        thread::scope(|scope| {
            let mut workers = Vec::new();
            for _ in 0..CALLERS {
                let cache = Arc::clone(&cache);
                let calls = Arc::clone(&calls);
                let start = Arc::clone(&start);
                workers.push(scope.spawn(move || {
                    start.wait();
                    *get_or_try_cache(&cache, || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(42)
                    })
                    .unwrap()
                }));
            }

            for worker in workers {
                assert_eq!(worker.join().unwrap(), 42);
            }
        });

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn waiter_retries_after_a_resource_failure() {
        let cache = Arc::new(SchemaCacheCell::<usize>::new());
        let retry_calls = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        thread::scope(|scope| {
            let first_cache = Arc::clone(&cache);
            let first = scope.spawn(move || {
                get_or_try_cache(&first_cache, || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Err(BinaryError::memory_error("temporary allocation failure"))
                })
                .copied()
            });

            entered_rx.recv().unwrap();
            let second_cache = Arc::clone(&cache);
            let second_calls = Arc::clone(&retry_calls);
            let second = scope.spawn(move || {
                *get_or_try_cache(&second_cache, || {
                    second_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(7)
                })
                .unwrap()
            });

            release_tx.send(()).unwrap();
            assert!(matches!(
                first.join().unwrap(),
                Err(BinaryError::MemoryError(_))
            ));
            assert_eq!(second.join().unwrap(), 7);
        });

        assert_eq!(retry_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn initializer_panic_leaves_the_schema_cache_retryable() {
        let cache = Arc::new(SchemaCacheCell::<usize>::new());
        let panic_target = Arc::clone(&cache);
        assert!(
            thread::spawn(move || {
                let _ = get_or_try_cache(&panic_target, || -> Result<usize> {
                    panic!("panic during schema initialization");
                });
            })
            .join()
            .is_err()
        );

        assert_eq!(*get_or_try_cache(&cache, || Ok(11)).unwrap(), 11);
    }

    #[test]
    fn deterministic_schema_failure_is_cached() {
        let cache = SchemaCacheCell::<usize>::new();
        let calls = AtomicUsize::new(0);

        for _ in 0..2 {
            let error = get_or_try_cache(&cache, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(BinaryError::invalid_data("malformed schema"))
            })
            .unwrap_err();
            assert!(matches!(error, BinaryError::InvalidData(_)));
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
