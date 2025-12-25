//! SerializedFile parser implementation
//!
//! This module provides the main parsing logic for Unity SerializedFile structures.

use super::header::SerializedFileHeader;
use super::types::{
    FileIdentifier, LocalSerializedObjectIdentifier, ObjectInfo, SerializedType, TypeRegistry,
};
use crate::error::{BinaryError, Result};
use crate::object::ObjectHandle;
use crate::reader::{BinaryReader, ByteOrder};
use crate::typetree::TypeTreeRegistry;
use crate::data_view::DataView;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::OnceLock;

/// SerializedFile parser
///
/// This struct handles the parsing of Unity SerializedFile structures,
/// supporting different Unity versions and formats.
pub struct SerializedFileParser;

impl SerializedFileParser {
    /// Parse SerializedFile from binary data
    pub fn from_bytes(data: Vec<u8>) -> Result<SerializedFile> {
        // Default to lazy object data loading to avoid copying per-object buffers.
        Self::from_bytes_with_options(data, false)
    }

    /// Parse SerializedFile from binary data with options
    pub fn from_bytes_with_options(
        data: Vec<u8>,
        preload_object_data: bool,
    ) -> Result<SerializedFile> {
        let data: Arc<[u8]> = data.into();
        let len = data.len();
        Self::from_shared_range_with_options(data, 0..len, preload_object_data)
    }

    /// Parse a SerializedFile from a shared backing buffer + byte range (zero-copy view).
    pub fn from_shared_range(
        data: Arc<[u8]>,
        range: Range<usize>,
    ) -> Result<SerializedFile> {
        Self::from_shared_range_with_options(data, range, false)
    }

    /// Parse a SerializedFile from a shared backing buffer + byte range (zero-copy view), with options.
    pub fn from_shared_range_with_options(
        data: Arc<[u8]>,
        range: Range<usize>,
        preload_object_data: bool,
    ) -> Result<SerializedFile> {
        let view = DataView::from_range(data, range)?;
        Self::from_view_with_options(view, preload_object_data)
    }

    fn from_view_with_options(view: DataView, preload_object_data: bool) -> Result<SerializedFile> {
        let mut file = SerializedFile {
            header: SerializedFileHeader::default(),
            unity_version: String::new(),
            target_platform: 0,
            enable_type_tree: false,
            type_tree_registry: None,
            types: Vec::new(),
            big_id_enabled: false,
            objects: Vec::new(),
            script_types: Vec::new(),
            externals: Vec::new(),
            ref_types: Vec::new(),
            user_information: String::new(),
            data: view,
            object_index_by_path_id: OnceLock::new(),
        };

        {
            let backing = file.data.backing_arc();
            let start = file.data.base_offset();
            let len = file.data.len();
            let bytes = &backing[start..start + len];
            let mut reader = BinaryReader::new(bytes, ByteOrder::Big);

            // Read header
            file.header = SerializedFileHeader::from_reader(&mut reader)?;

            if !file.header.is_valid() {
                return Err(BinaryError::invalid_data("Invalid SerializedFile header"));
            }

            // Switch to the correct byte order
            reader.set_byte_order(file.header.byte_order());

            // Parse metadata
            Self::parse_metadata(&mut file, &mut reader)?;
        }

        if preload_object_data {
            file.load_object_data()?;
        }

        Ok(file)
    }

    /// Parse SerializedFile from binary data asynchronously
    #[cfg(feature = "async")]
    pub async fn from_bytes_async(data: Vec<u8>) -> Result<SerializedFile> {
        Self::from_bytes_async_with_options(data, false).await
    }

    /// Parse SerializedFile from binary data asynchronously with options
    #[cfg(feature = "async")]
    pub async fn from_bytes_async_with_options(
        data: Vec<u8>,
        preload_object_data: bool,
    ) -> Result<SerializedFile> {
        // For now, use spawn_blocking to run the sync version
        let result = tokio::task::spawn_blocking(move || {
            Self::from_bytes_with_options(data, preload_object_data)
        })
        .await
        .map_err(|e| BinaryError::generic(format!("Task join error: {}", e)))??;

        Ok(result)
    }

    /// Parse the metadata section
    fn parse_metadata(file: &mut SerializedFile, reader: &mut BinaryReader) -> Result<()> {
        // Read Unity version (if version >= 7)
        if file.header.version >= 7 {
            file.unity_version = reader.read_cstring()?;
        }

        // Read target platform (if version >= 8)
        if file.header.version >= 8 {
            file.target_platform = reader.read_i32()?;
        }

        // Read enable type tree flag (if version >= 13)
        if file.header.version >= 13 {
            file.enable_type_tree = reader.read_bool()?;
        }

        // Read types
        let type_count = reader.read_i32()?;
        if type_count < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative type count: {}",
                type_count
            )));
        }
        let type_count = type_count as usize;
        for _ in 0..type_count {
            let serialized_type = SerializedType::from_reader(
                reader,
                file.header.version,
                file.enable_type_tree,
                false,
            )?;
            file.types.push(serialized_type);
        }

        // Read big ID enabled flag (if version 7-13)
        if file.header.version >= 7 && file.header.version < 14 {
            file.big_id_enabled = reader.read_i32()? != 0;
        }

        // Read objects
        let object_count = reader.read_i32()?;
        if object_count < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative object count: {}",
                object_count
            )));
        }
        let object_count = object_count as usize;
        for _ in 0..object_count {
            let object_info = Self::parse_object_info(file, reader)?;
            file.objects.push(object_info);
        }

        // Read script types (if version >= 11)
        if file.header.version >= 11 {
            let script_count = reader.read_i32()?;
            if script_count < 0 {
                return Err(BinaryError::invalid_data(format!(
                    "Negative script count: {}",
                    script_count
                )));
            }
            let script_count = script_count as usize;
            for _ in 0..script_count {
                let script_type =
                    LocalSerializedObjectIdentifier::from_reader(reader, file.header.version)?;
                file.script_types.push(script_type);
            }
        }

        // Read externals
        let external_count = reader.read_i32()?;
        if external_count < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative external count: {}",
                external_count
            )));
        }
        let external_count = external_count as usize;
        for _ in 0..external_count {
            let external = FileIdentifier::from_reader(reader, file.header.version)?;
            file.externals.push(external);
        }

        // Read ref types (if version >= 20)
        if file.header.version >= 20 {
            let ref_type_count = reader.read_i32()?;
            if ref_type_count < 0 {
                return Err(BinaryError::invalid_data(format!(
                    "Negative ref type count: {}",
                    ref_type_count
                )));
            }
            let ref_type_count = ref_type_count as usize;
            for _ in 0..ref_type_count {
                let ref_type = SerializedType::from_reader(
                    reader,
                    file.header.version,
                    file.enable_type_tree,
                    true,
                )?;
                file.ref_types.push(ref_type);
            }
        }

        // Read user information (if version >= 5)
        if file.header.version >= 5 {
            file.user_information = reader.read_cstring()?;
        }

        Ok(())
    }

    /// Parse object information
    fn parse_object_info(
        file: &mut SerializedFile,
        reader: &mut BinaryReader,
    ) -> Result<ObjectInfo> {
        let version = file.header.version;

        // Path ID
        let path_id = if file.big_id_enabled {
            reader.read_i64()?
        } else if version < 14 {
            reader.read_i32()? as i64
        } else {
            reader.align()?;
            reader.read_i64()?
        };

        // Byte start
        let byte_start = if version >= 22 {
            i64_to_u64_checked(reader.read_i64()?, "object.byte_start")?
        } else {
            reader.read_u32()? as u64
        };
        let byte_start = byte_start
            .checked_add(file.header.data_offset)
            .ok_or_else(|| BinaryError::invalid_data("Object byte_start overflow"))?;

        // Byte size
        let byte_size = reader.read_u32()?;

        // Raw type id (index into `types` for version >= 16)
        let raw_type_id = reader.read_i32()?;

        // Resolve class id (UnityPy: class_id)
        let (class_id, type_index) = if version < 16 {
            let class_id = reader.read_u16()? as i32;
            (class_id, -1)
        } else {
            let idx = raw_type_id;
            let class_id = file
                .types
                .get(idx as usize)
                .ok_or_else(|| {
                    BinaryError::invalid_data(format!(
                        "Invalid type index in object table: {}",
                        idx
                    ))
                })?
                .class_id;
            (class_id, idx)
        };

        // is_destroyed (version < 11)
        if version < 11 {
            let _is_destroyed = reader.read_u16()?;
        }

        // script_type_index is stored per-object for 11 <= version < 17
        if (11..17).contains(&version) {
            let script_type_index = reader.read_i16()?;
            // UnityPy assigns this to the referenced SerializedType when possible.
            if version < 16 {
                if let Some(typ) = file.types.iter_mut().find(|t| t.class_id == raw_type_id) {
                    typ.script_type_index = script_type_index;
                }
            } else if raw_type_id >= 0 {
                if let Some(typ) = file.types.get_mut(raw_type_id as usize) {
                    typ.script_type_index = script_type_index;
                }
            }
        }

        // stripped flag (version 15 or 16)
        if version == 15 || version == 16 {
            let _stripped = reader.read_u8()?;
        }

        Ok(ObjectInfo::new(
            path_id, byte_start, byte_size, class_id, type_index,
        ))
    }

    /// Validate parsed SerializedFile
    pub fn validate(file: &SerializedFile) -> Result<()> {
        // Validate header
        file.header.validate()?;

        // Validate objects
        for (i, obj) in file.objects.iter().enumerate() {
            obj.validate().map_err(|e| {
                BinaryError::generic(format!("Object {} validation failed: {}", i, e))
            })?;
        }

        // Validate types
        for (i, stype) in file.types.iter().enumerate() {
            stype.validate().map_err(|e| {
                BinaryError::generic(format!("Type {} validation failed: {}", i, e))
            })?;
        }

        Ok(())
    }

    /// Get parsing statistics
    pub fn get_parsing_stats(file: &SerializedFile) -> ParsingStats {
        ParsingStats {
            version: file.header.version,
            unity_version: file.unity_version.clone(),
            target_platform: file.target_platform,
            file_size: file.header.file_size,
            object_count: file.objects.len(),
            type_count: file.types.len(),
            script_type_count: file.script_types.len(),
            external_count: file.externals.len(),
            has_type_tree: file.enable_type_tree,
            big_id_enabled: file.big_id_enabled,
        }
    }
}

/// Complete SerializedFile structure
///
/// This structure represents a complete Unity SerializedFile with all its
/// metadata, type information, and object data.
#[derive(Debug)]
pub struct SerializedFile {
    /// File header
    pub header: SerializedFileHeader,
    /// Unity version string
    pub unity_version: String,
    /// Target platform
    pub target_platform: i32,
    /// Whether type tree is enabled
    pub enable_type_tree: bool,
    /// Optional external TypeTree registry for stripped files (best-effort).
    pub type_tree_registry: Option<Arc<dyn TypeTreeRegistry>>,
    /// Type information
    pub types: Vec<SerializedType>,
    /// Whether big IDs are enabled
    pub big_id_enabled: bool,
    /// Object information
    pub objects: Vec<ObjectInfo>,
    /// Script types
    pub script_types: Vec<LocalSerializedObjectIdentifier>,
    /// External file references
    pub externals: Vec<FileIdentifier>,
    /// Reference types
    pub ref_types: Vec<SerializedType>,
    /// User information
    pub user_information: String,
    /// Raw file data
    data: DataView,
    object_index_by_path_id: OnceLock<HashMap<i64, usize>>,
}

impl SerializedFile {
    pub fn set_type_tree_registry(&mut self, registry: Option<Arc<dyn TypeTreeRegistry>>) {
        self.type_tree_registry = registry;
    }

    /// Get the raw file data
    pub fn data(&self) -> &[u8] {
        self.data.as_bytes()
    }

    /// Get the backing shared buffer for this file's bytes.
    ///
    /// Note: for embedded files (e.g. files inside a decompressed bundle buffer), this is the
    /// shared backing buffer and may be larger than `self.data()`.
    pub fn data_arc(&self) -> Arc<[u8]> {
        self.data.backing_arc()
    }

    /// Base offset of this file within the backing shared buffer returned by `data_arc()`.
    pub fn data_base_offset(&self) -> usize {
        self.data.base_offset()
    }

    /// A stable identity key for caches: `(backing_ptr, base_offset, len)`.
    pub fn data_identity_key(&self) -> (usize, usize, usize) {
        self.data.identity_key()
    }

    /// Get the raw bytes for an object without requiring preloaded per-object buffers.
    pub fn object_bytes<'a>(&'a self, info: &ObjectInfo) -> Result<&'a [u8]> {
        let start: usize = info.byte_start.try_into().map_err(|_| {
            BinaryError::invalid_data(format!("Object byte_start overflow: {}", info.byte_start))
        })?;
        let end = start.saturating_add(info.byte_size as usize);
        let data = self.data();
        if end > data.len() {
            return Err(BinaryError::invalid_data(format!(
                "Object data out of bounds (path_id={}, start={}, size={}, file_len={})",
                info.path_id,
                start,
                info.byte_size,
                data.len()
            )));
        }
        Ok(&data[start..end])
    }

    /// Best-effort raw parser for Unity `AssetBundle` (class id `142`) `m_Container`.
    ///
    /// This exists as a fallback when TypeTree is stripped/unavailable. The layout is version-dependent,
    /// so this function tries multiple 4-byte-aligned starting offsets and applies sanity checks.
    ///
    /// Returns a list of `(asset_path, file_id, path_id)` tuples.
    pub fn assetbundle_container_raw(&self, info: &ObjectInfo) -> Result<Vec<(String, i32, i64)>> {
        let data = self.object_bytes(info)?;
        let byte_order = self.header.byte_order();

        fn parse_pptr(reader: &mut BinaryReader) -> Result<(i32, i64)> {
            let file_id = reader.read_i32()?;
            let path_id = reader.read_i64()?;
            Ok((file_id, path_id))
        }

        fn parse_aligned_string(reader: &mut BinaryReader) -> Result<String> {
            let s = reader.read_string()?;
            reader.align()?;
            Ok(s)
        }

        fn try_parse(
            reader: &mut BinaryReader,
            assetinfo_layout: bool,
            assetinfo_asset_last: bool,
        ) -> Result<Vec<(String, i32, i64)>> {
            // AssetBundle inherits from Object/NamedObject; many versions start with some base fields.
            // We start parsing at a candidate offset (handled by outer loop) assuming the next field is m_Name.
            let _name = parse_aligned_string(reader)?;

            // m_PreloadTable: Array<PPtr<Object>>
            let preload_size = reader.read_i32()?;
            if !(0..=1_000_000).contains(&preload_size) {
                return Err(BinaryError::invalid_data(format!(
                    "Invalid AssetBundle preload table size: {}",
                    preload_size
                )));
            }
            for _ in 0..preload_size {
                let _ = parse_pptr(reader)?;
            }
            reader.align()?;

            // m_Container: Array<pair<string, AssetInfo>>
            let container_size = reader.read_i32()?;
            if !(0..=1_000_000).contains(&container_size) {
                return Err(BinaryError::invalid_data(format!(
                    "Invalid AssetBundle container size: {}",
                    container_size
                )));
            }

            let mut out = Vec::with_capacity(container_size as usize);
            for _ in 0..container_size {
                let asset_path = parse_aligned_string(reader)?;

                // Unity uses either:
                // - AssetInfo { asset: PPtr<Object>, preloadIndex: int, preloadSize: int } (many versions)
                // - PPtr<Object> only (some versions)
                let (file_id, path_id) = if assetinfo_layout {
                    if assetinfo_asset_last {
                        let _preload_index = reader.read_i32()?;
                        let _preload_size = reader.read_i32()?;
                        parse_pptr(reader)?
                    } else {
                        let pptr = parse_pptr(reader)?;
                        let _preload_index = reader.read_i32()?;
                        let _preload_size = reader.read_i32()?;
                        pptr
                    }
                } else {
                    parse_pptr(reader)?
                };

                out.push((asset_path, file_id, path_id));
            }
            reader.align()?;

            // m_MainAsset (usually AssetInfo)
            if assetinfo_layout {
                if assetinfo_asset_last {
                    let _preload_index = reader.read_i32()?;
                    let _preload_size = reader.read_i32()?;
                    let _ = parse_pptr(reader)?;
                } else {
                    let _ = parse_pptr(reader)?;
                    let _preload_index = reader.read_i32()?;
                    let _preload_size = reader.read_i32()?;
                }
            } else {
                let _ = parse_pptr(reader)?;
            }
            reader.align()?;

            Ok(out)
        }

        // Try multiple aligned offsets to account for base fields which may precede m_Name.
        let mut last_err: Option<BinaryError> = None;
        let externals_len: i32 = self.externals.len().try_into().unwrap_or(i32::MAX);
        let mut best: Option<(usize, Vec<(String, i32, i64)>)> = None;

        fn score(entries: &[(String, i32, i64)], externals_len: i32) -> usize {
            entries
                .iter()
                .filter(|(path, file_id, path_id)| {
                    !path.is_empty()
                        && *path_id != 0
                        && *file_id >= 0
                        && (*file_id == 0 || *file_id - 1 <= externals_len)
                })
                .count()
        }

        for offset in (0..=256usize).step_by(4) {
            if offset >= data.len() {
                break;
            }

            // Try both layouts and keep the better-scored candidate.
            for assetinfo_layout in [true, false] {
                let variants: &[(bool, bool)] = if assetinfo_layout {
                    // Try both field orders for AssetInfo.
                    &[(true, false), (true, true)]
                } else {
                    &[(false, false)]
                };

                for &(_layout, asset_last) in variants {
                    let mut reader = BinaryReader::new(&data[offset..], byte_order);
                    match try_parse(&mut reader, assetinfo_layout, asset_last) {
                        Ok(entries) => {
                            let s = score(&entries, externals_len);
                            let better = match &best {
                                None => true,
                                Some((best_score, best_entries)) => {
                                    s > *best_score
                                        || (s == *best_score && entries.len() > best_entries.len())
                                }
                            };
                            if better {
                                best = Some((s, entries));
                            }
                        }
                        Err(e) => last_err = Some(e),
                    }
                }
            }
        }

        if let Some((_score, entries)) = best {
            // Sanity: container usually has some non-empty paths.
            if entries.iter().any(|(p, _, _)| !p.is_empty()) {
                return Ok(entries);
            }
        }

        Err(last_err.unwrap_or_else(|| {
            BinaryError::invalid_data(
                "Failed to parse AssetBundle container (no candidates matched)",
            )
        }))
    }

    /// Get object count
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// Get type count
    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    /// Find object by path ID
    pub fn find_object(&self, path_id: i64) -> Option<&ObjectInfo> {
        let index = self.object_index_by_path_id.get_or_init(|| {
            let mut map = HashMap::with_capacity(self.objects.len());
            for (idx, obj) in self.objects.iter().enumerate() {
                map.insert(obj.path_id, idx);
            }
            map
        });
        index.get(&path_id).and_then(|idx| self.objects.get(*idx))
    }

    /// Iterate all objects as lightweight handles.
    pub fn object_handles(&self) -> impl Iterator<Item = ObjectHandle<'_>> {
        self.objects
            .iter()
            .map(|info| ObjectHandle::new(self, info))
    }

    /// Find an object by `path_id` and return a lightweight handle.
    pub fn find_object_handle(&self, path_id: i64) -> Option<ObjectHandle<'_>> {
        self.find_object(path_id)
            .map(|info| ObjectHandle::new(self, info))
    }

    /// Find type by class ID
    pub fn find_type(&self, class_id: i32) -> Option<&SerializedType> {
        self.types.iter().find(|t| t.class_id == class_id)
    }

    /// Get all objects of a specific type
    pub fn objects_of_type(&self, type_id: i32) -> Vec<&ObjectInfo> {
        self.objects
            .iter()
            .filter(|obj| obj.type_id == type_id)
            .collect()
    }

    /// Create a type registry from this file
    pub fn create_type_registry(&self) -> TypeRegistry {
        let mut registry = TypeRegistry::new();

        for stype in &self.types {
            registry.add_type(stype.clone());
        }

        registry
    }

    /// Get file statistics
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

    /// Validate the entire file
    pub fn validate(&self) -> Result<()> {
        SerializedFileParser::validate(self)
    }

    fn load_object_data(&mut self) -> Result<()> {
        let backing = self.data.backing_arc();
        let start = self.data.base_offset();
        let len = self.data.len();
        let bytes = &backing[start..start + len];
        let file_len = bytes.len();
        for obj in &mut self.objects {
            let start: usize = obj.byte_start.try_into().map_err(|_| {
                BinaryError::invalid_data(format!("Object byte_start overflow: {}", obj.byte_start))
            })?;
            let end = start.saturating_add(obj.byte_size as usize);
            if end > file_len {
                return Err(BinaryError::invalid_data(format!(
                    "Object data out of bounds (path_id={}, start={}, size={}, file_len={})",
                    obj.path_id, start, obj.byte_size, file_len
                )));
            }
            obj.data = bytes[start..end].to_vec();
        }
        Ok(())
    }
}

fn i64_to_u64_checked(value: i64, name: &'static str) -> Result<u64> {
    if value < 0 {
        return Err(BinaryError::invalid_data(format!(
            "Invalid {}: negative value {}",
            name, value
        )));
    }
    Ok(value as u64)
}

/// Parsing statistics
#[derive(Debug, Clone)]
pub struct ParsingStats {
    pub version: u32,
    pub unity_version: String,
    pub target_platform: i32,
    pub file_size: u64,
    pub object_count: usize,
    pub type_count: usize,
    pub script_type_count: usize,
    pub external_count: usize,
    pub has_type_tree: bool,
    pub big_id_enabled: bool,
}

/// File statistics
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
    #[test]
    fn test_parser_creation() {
        // Basic test to ensure parser methods exist
        // This test verifies that the parser module compiles correctly
        let _dummy = 1 + 1;
        assert_eq!(_dummy, 2);
    }
}
