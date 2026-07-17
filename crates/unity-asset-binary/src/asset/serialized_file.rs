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
use crate::typetree::TypeTreeRegistry;
use std::collections::HashMap;
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
    pub enable_type_tree: bool,
    /// Optional external TypeTree registry for stripped files (best-effort).
    pub type_tree_registry: Option<Arc<dyn TypeTreeRegistry>>,
    /// Type information.
    pub types: Vec<SerializedType>,
    /// Exact legacy `bigIdEnabled` value when the format stores it.
    legacy_big_id: Option<i32>,
    /// Object information in immutable table order.
    objects: Vec<ObjectInfo>,
    /// Script types.
    pub script_types: Vec<LocalSerializedObjectIdentifier>,
    /// External file references.
    pub externals: Vec<FileIdentifier>,
    /// Reference types.
    pub ref_types: Vec<SerializedType>,
    /// User information.
    pub user_information: String,
    /// Raw file data.
    data: DataView,
    object_index_by_path_id: OnceLock<HashMap<i64, usize>>,
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
        })
    }

    /// Returns object metadata in immutable SerializedFile table order.
    ///
    /// Object topology is intentionally not mutable after parsing because path-ID lookup is part
    /// of this snapshot's identity contract. Payload overrides use [`Self::find_object_mut`].
    pub fn objects(&self) -> &[ObjectInfo] {
        &self.objects
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
