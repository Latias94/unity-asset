//! Unity object representation and helpers.

use crate::asset::{ObjectInfo, SerializedFile, SerializedType};
use crate::error::{BinaryError, BinaryObjectReplacementError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use crate::reference::BinaryReferenceScan;
use crate::shared_bytes::SharedBytes;
use crate::typetree::{
    PPtrScanResult, TypeTree, TypeTreeParseMode, TypeTreeParseOptions, TypeTreeParseOutput,
    TypeTreeParseWarning, TypeTreeSchema, TypeTreeTraversalStats,
};
use crate::unity_objects::{GameObject, Transform};
use std::fmt::Write as _;
use std::io::Read;
use std::sync::Arc;
use unity_asset_core::{AssetLoadBudget, UnityClass, UnityValue};

/// Origin of the canonical schema selected for an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectSchemaOrigin {
    EmbeddedTypeTree,
    ExternalRegistry,
}

/// One object materialization together with the exact schema selection used to parse it.
#[derive(Debug)]
pub struct MaterializedObject {
    object: UnityObject,
    selected_schema: Option<SelectedObjectSchema>,
}

impl MaterializedObject {
    #[must_use]
    pub const fn object(&self) -> &UnityObject {
        &self.object
    }

    #[must_use]
    pub const fn schema(&self) -> Option<&TypeTreeSchema> {
        match &self.selected_schema {
            Some(selected) => Some(&selected.schema),
            None => None,
        }
    }

    #[must_use]
    pub const fn schema_origin(&self) -> Option<ObjectSchemaOrigin> {
        match &self.selected_schema {
            Some(selected) => Some(selected.origin),
            None => None,
        }
    }

    #[must_use]
    pub fn into_object(self) -> UnityObject {
        self.object
    }
}

/// A lightweight reference to a binary object within a [`SerializedFile`].
///
/// This is conceptually similar to UnityPy's `ObjectReader`: it carries just enough context
/// (file + object metadata) to parse the object on-demand.
#[derive(Debug, Clone, Copy)]
pub struct ObjectHandle<'a> {
    file: &'a SerializedFile,
    info: &'a ObjectInfo,
}

impl<'a> ObjectHandle<'a> {
    pub fn new(file: &'a SerializedFile, info: &'a ObjectInfo) -> Self {
        Self { file, info }
    }

    pub fn file(&self) -> &'a SerializedFile {
        self.file
    }

    pub fn info(&self) -> &'a ObjectInfo {
        self.info
    }

    pub fn path_id(&self) -> i64 {
        self.info.path_id()
    }

    pub fn class_id(&self) -> i32 {
        self.info.class_id()
    }

    pub fn byte_start(&self) -> u64 {
        self.info.byte_start()
    }

    pub fn byte_size(&self) -> u32 {
        self.info.byte_size()
    }

    /// Get the raw bytes for this object (preloaded if available, otherwise sliced from the file).
    pub fn raw_data(&self) -> Result<&'a [u8]> {
        if let Some(data) = self.info.loaded_data() {
            return Ok(data);
        }
        self.file.object_bytes(self.info)
    }

    /// Parse this object into an owned [`UnityObject`] under a caller-owned resource budget.
    pub fn read(&self, budget: &mut AssetLoadBudget) -> Result<UnityObject> {
        self.read_with_options(budget, TypeTreeParseOptions::default())
    }

    pub fn read_with_options(
        &self,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<UnityObject> {
        Ok(self
            .materialize_with_options(budget, options)?
            .into_object())
    }

    /// Parses the object and retains the exact schema selection used for that parse.
    pub fn materialize(&self, budget: &mut AssetLoadBudget) -> Result<MaterializedObject> {
        self.materialize_with_options(budget, TypeTreeParseOptions::default())
    }

    pub fn materialize_with_options(
        &self,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<MaterializedObject> {
        let selected = self.selected_schema(budget)?;
        let object = UnityObject::from_serialized_file_with_compiled_schema(
            self.file,
            self.info,
            selected.as_ref().map(|selected| &selected.schema),
            budget,
            options,
        )?;
        Ok(MaterializedObject {
            object,
            selected_schema: selected,
        })
    }

    /// Parses caller-provided replacement bytes with this object's canonical schema and identity.
    ///
    /// The replacement extent must contain exactly one complete object. The original
    /// [`ObjectInfo`] and SerializedFile backing remain untouched.
    pub fn read_replacement(
        &self,
        replacement: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> Result<UnityObject> {
        self.read_replacement_with_options(
            replacement,
            budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Strict,
            },
        )
    }

    /// Parses replacement bytes under an explicit TypeTree recovery policy.
    pub fn read_replacement_with_options(
        &self,
        replacement: &[u8],
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<UnityObject> {
        Ok(self
            .materialize_replacement_with_options(replacement, budget, options)?
            .into_object())
    }

    /// Parses replacement bytes and retains the exact schema selected for that parse.
    pub fn materialize_replacement(
        &self,
        replacement: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> Result<MaterializedObject> {
        self.materialize_replacement_with_options(
            replacement,
            budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Strict,
            },
        )
    }

    /// Parses replacement bytes and retains the schema under an explicit recovery policy.
    pub fn materialize_replacement_with_options(
        &self,
        replacement: &[u8],
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<MaterializedObject> {
        let selected =
            self.selected_schema(budget)?
                .ok_or(BinaryObjectReplacementError::MissingSchema {
                    path_id: self.path_id(),
                    class_id: self.class_id(),
                })?;
        let object = UnityObject::from_replacement_with_compiled_schema(
            self.file,
            self.info,
            replacement,
            &selected.schema,
            budget,
            options,
        )?;
        Ok(MaterializedObject {
            object,
            selected_schema: Some(selected),
        })
    }

    /// Reads and parses one exact replacement extent without retaining an intermediate copy.
    ///
    /// The caller supplies the parser-proven payload length. The retained raw payload is the same
    /// allocation filled from `reader`; it is not copied again after TypeTree traversal.
    pub fn materialize_replacement_from_reader(
        &self,
        reader: &mut impl Read,
        replacement_len: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<MaterializedObject> {
        self.materialize_replacement_from_reader_with_options(
            reader,
            replacement_len,
            budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Strict,
            },
        )
    }

    /// Reads and parses one exact replacement extent under an explicit recovery policy.
    pub fn materialize_replacement_from_reader_with_options(
        &self,
        reader: &mut impl Read,
        replacement_len: usize,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<MaterializedObject> {
        // Resolve the schema before advancing the reader. A caller may then deliberately fall
        // back to `materialize_raw_replacement_from_reader` on MissingSchema.
        let selected =
            self.selected_schema(budget)?
                .ok_or(BinaryObjectReplacementError::MissingSchema {
                    path_id: self.path_id(),
                    class_id: self.class_id(),
                })?;
        let replacement = read_owned_replacement(reader, replacement_len, budget)?;
        let object = UnityObject::from_owned_replacement_with_compiled_schema(
            self.file,
            self.info,
            replacement,
            &selected.schema,
            budget,
            options,
        )?;
        Ok(MaterializedObject {
            object,
            selected_schema: Some(selected),
        })
    }

    /// Reads one exact replacement extent when no canonical TypeTree is available.
    ///
    /// Identity, class, byte order, and raw bytes remain authoritative. The semantic class is
    /// intentionally property-free and the payload provenance records that no schema was used.
    pub fn materialize_raw_replacement_from_reader(
        &self,
        reader: &mut impl Read,
        replacement_len: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<MaterializedObject> {
        validate_raw_replacement_len(replacement_len)?;
        let replacement = read_owned_replacement(reader, replacement_len, budget)?;
        let object =
            UnityObject::from_owned_raw_replacement(self.file, self.info, replacement, budget)?;
        Ok(MaterializedObject {
            object,
            selected_schema: None,
        })
    }

    /// Returns the canonical TypeTree schema selected for this object.
    ///
    /// Schemas backed by this file's SerializedType table are shared across every handle for the
    /// same type. External registry results are compiled per lookup because registries may change
    /// the returned tree without exposing a revision identity.
    pub fn schema(&self, budget: &mut AssetLoadBudget) -> Result<Option<TypeTreeSchema>> {
        Ok(self
            .selected_schema(budget)?
            .map(|selected| selected.schema))
    }

    /// Reports where [`Self::schema`] obtains its canonical TypeTree without compiling it.
    #[must_use]
    pub fn schema_origin(&self) -> Option<ObjectSchemaOrigin> {
        type_tree_for_object(self.file, self.info).map(|source| match source {
            TypeTreeSource::Internal(_) => ObjectSchemaOrigin::EmbeddedTypeTree,
            TypeTreeSource::External(_) => ObjectSchemaOrigin::ExternalRegistry,
        })
    }

    fn selected_schema(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Option<SelectedObjectSchema>> {
        let Some(source) = type_tree_for_object(self.file, self.info) else {
            return Ok(None);
        };
        let (schema, origin) = match source {
            TypeTreeSource::Internal(type_index) => (
                self.file.cached_internal_schema(type_index, budget)?,
                ObjectSchemaOrigin::EmbeddedTypeTree,
            ),
            TypeTreeSource::External(tree) => (
                self.file.compile_schema(tree.as_ref(), budget)?,
                ObjectSchemaOrigin::ExternalRegistry,
            ),
        };
        Ok(Some(SelectedObjectSchema { schema, origin }))
    }

    /// Peek the object's name (`m_Name`/`name`) without parsing the full TypeTree.
    ///
    /// This mirrors UnityPy's `ObjectReader.peek_name()` behavior by parsing only a prefix of the
    /// root TypeTree until the name field, when possible.
    pub fn peek_name(&self, budget: &mut AssetLoadBudget) -> Result<Option<String>> {
        self.peek_name_with_options(
            budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Lenient,
            },
        )
    }

    pub fn peek_name_with_options(
        &self,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<Option<String>> {
        let Some(schema) = self.schema(budget)? else {
            return Ok(None);
        };
        let Some((prefix_len, field)) = name_peek_prefix(&schema) else {
            return Ok(None);
        };

        let bytes = self.raw_data()?;
        let mut reader = BinaryReader::new(bytes, self.file.header.byte_order());
        let mut out = schema.read_object_prefix(&mut reader, budget, options, prefix_len)?;

        match out.properties.shift_remove(field) {
            Some(UnityValue::String(value)) => Ok(Some(value)),
            _ => Ok(None),
        }
    }

    /// Scan TypeTree-based object bytes and collect `PPtr` references (`fileID`, `pathID`) without
    /// allocating a full parsed `UnityValue` tree.
    pub fn scan_pptrs(&self, budget: &mut AssetLoadBudget) -> Result<Option<PPtrScanResult>> {
        let Some(schema) = self.schema(budget)? else {
            return Ok(None);
        };
        let bytes = self.raw_data()?;
        let mut reader = BinaryReader::new(bytes, self.file.header.byte_order());
        Ok(Some(schema.scan_pptrs(&mut reader, budget)?))
    }

    /// Scans caller-provided replacement bytes for PPtrs without materializing a UnityValue tree.
    ///
    /// Returns `None` when this object has no canonical TypeTree schema. A successful scan
    /// consumes the complete replacement extent.
    pub fn scan_replacement_pptrs(
        &self,
        replacement: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> Result<Option<PPtrScanResult>> {
        let Some(schema) = self.schema(budget)? else {
            return Ok(None);
        };
        parse_replacement_extent(
            replacement,
            self.file.header.byte_order(),
            self.path_id(),
            |reader| schema.scan_pptrs(reader, budget),
        )
        .map(Some)
    }

    /// Scans this object's canonical TypeTree without materializing a `UnityValue` tree.
    ///
    /// Occurrences retain their traversal order, field paths, null pointers, and raw file IDs.
    pub fn scan_reference_occurrences(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Option<BinaryReferenceScan>> {
        self.scan_reference_occurrences_with_options(
            budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Strict,
            },
        )
    }

    /// Scans this object's references under an explicit TypeTree recovery policy.
    pub fn scan_reference_occurrences_with_options(
        &self,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<Option<BinaryReferenceScan>> {
        let Some(schema) = self.schema(budget)? else {
            return Ok(None);
        };
        let bytes = self.raw_data()?;
        let mut reader = BinaryReader::new(bytes, self.file.header.byte_order());
        Ok(Some(schema.scan_reference_occurrences_with_options(
            &mut reader,
            budget,
            options,
        )?))
    }

    /// Scans path-aware references from replacement bytes without materializing a UnityValue tree.
    pub fn scan_replacement_reference_occurrences(
        &self,
        replacement: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> Result<Option<BinaryReferenceScan>> {
        self.scan_replacement_reference_occurrences_with_options(
            replacement,
            budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Strict,
            },
        )
    }

    /// Scans path-aware replacement references under an explicit TypeTree recovery policy.
    pub fn scan_replacement_reference_occurrences_with_options(
        &self,
        replacement: &[u8],
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<Option<BinaryReferenceScan>> {
        let Some(schema) = self.schema(budget)? else {
            return Ok(None);
        };
        parse_replacement_extent(
            replacement,
            self.file.header.byte_order(),
            self.path_id(),
            |reader| schema.scan_reference_occurrences_with_options(reader, budget, options),
        )
        .map(Some)
    }
}

impl SerializedFile {
    /// Iterate all objects as lightweight handles.
    pub fn object_handles(&self) -> impl Iterator<Item = ObjectHandle<'_>> {
        self.objects()
            .iter()
            .map(|info| ObjectHandle::new(self, info))
    }

    /// Find an object by `path_id` and return a lightweight handle.
    pub fn find_object_handle(&self, path_id: i64) -> Option<ObjectHandle<'_>> {
        self.find_object(path_id)
            .map(|info| ObjectHandle::new(self, info))
    }
}

#[derive(Debug, Clone)]
enum ObjectBytes {
    Empty,
    Inline(Vec<u8>),
    Shared {
        data: SharedBytes,
        start: usize,
        end: usize,
    },
}

impl ObjectBytes {
    fn copy_from_slice(bytes: &[u8], budget: &mut AssetLoadBudget) -> Result<Self> {
        let amount = u64::try_from(bytes.len()).map_err(|_| {
            BinaryError::memory_error("Owned object payload length does not fit in u64")
        })?;
        budget.check_bytes(amount)?;
        let mut copy = Vec::new();
        copy.try_reserve_exact(bytes.len()).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {} bytes for an owned object payload: {error}",
                bytes.len()
            ))
        })?;
        copy.extend_from_slice(bytes);
        budget.consume_bytes(amount)?;
        Ok(Self::Inline(copy))
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            ObjectBytes::Empty => &[],
            ObjectBytes::Inline(bytes) => bytes.as_slice(),
            ObjectBytes::Shared { data, start, end } => &data.as_bytes()[*start..*end],
        }
    }
}

fn read_owned_replacement(
    reader: &mut impl Read,
    replacement_len: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>> {
    let amount = u64::try_from(replacement_len)
        .map_err(|_| BinaryError::memory_error("Replacement payload length does not fit in u64"))?;
    budget.check_bytes(amount)?;
    let mut replacement = Vec::new();
    replacement
        .try_reserve_exact(replacement_len)
        .map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {replacement_len} bytes for a replacement payload: {error}"
            ))
        })?;
    let retained = u64::try_from(replacement.capacity()).map_err(|_| {
        BinaryError::memory_error("Replacement payload capacity does not fit in u64")
    })?;
    budget.check_bytes(retained)?;
    budget.consume_bytes(retained)?;
    replacement.resize(replacement_len, 0);
    reader.read_exact(&mut replacement)?;
    Ok(replacement)
}

fn validate_raw_replacement_len(replacement_len: usize) -> Result<()> {
    u32::try_from(replacement_len)
        .map(|_| ())
        .map_err(|_| BinaryObjectReplacementError::RawPayloadTooLarge {
            length: replacement_len,
        })
        .map_err(Into::into)
}

/// Origin of the payload currently owned or referenced by a [`UnityObject`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectPayloadProvenance {
    /// Bytes are a range of the parsed SerializedFile's immutable backing image.
    Committed,
    /// Bytes came from the authoritative `ObjectInfo::loaded_data` payload.
    Loaded,
    /// Bytes were supplied to an [`ObjectHandle`] replacement API and parsed with a TypeTree.
    TypedReplacement,
    /// Bytes were supplied to an [`ObjectHandle`] replacement API without an available TypeTree.
    RawReplacement,
    /// The object was constructed independently of a SerializedFile payload.
    Synthetic,
}

/// A parsed Unity object.
///
/// This is an owned wrapper which carries:
/// - the raw `ObjectInfo` (from `asset` module)
/// - the parsed `UnityClass` properties (best-effort)
#[derive(Debug, Clone)]
pub struct UnityObject {
    pub info: ObjectInfo,
    pub class: UnityClass,
    byte_order: ByteOrder,
    raw: ObjectBytes,
    payload_provenance: ObjectPayloadProvenance,
    typetree_warnings: Vec<TypeTreeParseWarning>,
    typetree_stats: TypeTreeTraversalStats,
}

impl UnityObject {
    /// Create a UnityObject from an already-parsed UnityClass (used by tests and higher-level code).
    pub fn from_info_and_class(info: ObjectInfo, class: UnityClass) -> Self {
        Self {
            byte_order: ByteOrder::Little,
            info,
            class,
            raw: ObjectBytes::Empty,
            payload_provenance: ObjectPayloadProvenance::Synthetic,
            typetree_warnings: Vec::new(),
            typetree_stats: TypeTreeTraversalStats::default(),
        }
    }

    /// Create a UnityObject from raw bytes without TypeTree information.
    ///
    /// For large objects, this intentionally avoids expanding all bytes into a `UnityValue::Array`
    /// to reduce memory pressure and parsing time; use `raw_data()` instead.
    pub fn from_raw(class_id: i32, path_id: i64, data: Vec<u8>) -> Result<Self> {
        let byte_size = u32::try_from(data.len()).map_err(|_| {
            BinaryError::invalid_data(format!(
                "Raw object payload is too large for the u32 wire size: {} bytes",
                data.len()
            ))
        })?;
        let info = ObjectInfo::for_standalone_class(path_id, 0, byte_size, class_id)?;
        let raw = ObjectBytes::Inline(data);
        let class = UnityClass::new(class_id, class_name_from_id(class_id), path_id.to_string());
        Ok(Self {
            info,
            class,
            byte_order: ByteOrder::Little,
            raw,
            payload_provenance: ObjectPayloadProvenance::Synthetic,
            typetree_warnings: Vec::new(),
            typetree_stats: TypeTreeTraversalStats::default(),
        })
    }

    /// Create a UnityObject from a SerializedFile + ObjectInfo, using TypeTree when available.
    pub fn from_serialized_file(
        file: &SerializedFile,
        info: &ObjectInfo,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self> {
        Self::from_serialized_file_with_options(file, info, budget, TypeTreeParseOptions::default())
    }

    pub fn from_serialized_file_with_options(
        file: &SerializedFile,
        info: &ObjectInfo,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<Self> {
        ObjectHandle::new(file, info).read_with_options(budget, options)
    }

    fn from_serialized_file_with_compiled_schema(
        file: &SerializedFile,
        info: &ObjectInfo,
        schema: Option<&TypeTreeSchema>,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<Self> {
        let class_id = info.class_id();
        let byte_order = file.header.byte_order();
        let anchor = anchor_from_path_id(info.path_id(), budget)?;
        let class_name = class_name_from_id_with_budget(class_id, budget)?;
        let (class, warnings, stats) = match schema {
            Some(schema) => {
                let out = parse_object_data(file, info, byte_order, schema, budget, options)?;
                (
                    UnityClass::with_properties(class_id, class_name, anchor, out.properties),
                    out.warnings,
                    out.stats,
                )
            }
            _ => (
                UnityClass::new(class_id, class_name, anchor),
                Vec::new(),
                TypeTreeTraversalStats::default(),
            ),
        };
        let raw = owned_object_bytes(file, info, budget)?;
        let payload_provenance = if info.loaded_data().is_some() {
            ObjectPayloadProvenance::Loaded
        } else {
            ObjectPayloadProvenance::Committed
        };

        Ok(Self {
            info: info.clone_without_loaded_data(),
            class,
            byte_order,
            raw,
            payload_provenance,
            typetree_warnings: warnings,
            typetree_stats: stats,
        })
    }

    fn from_replacement_with_compiled_schema(
        file: &SerializedFile,
        info: &ObjectInfo,
        replacement: &[u8],
        schema: &TypeTreeSchema,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<Self> {
        let byte_order = file.header.byte_order();
        let out = parse_replacement_extent(replacement, byte_order, info.path_id(), |reader| {
            schema.read_object(reader, budget, options)
        })?;
        if !out.complete {
            return Err(BinaryObjectReplacementError::Incomplete {
                path_id: info.path_id(),
                consumed: replacement.len(),
            }
            .into());
        }
        let class_name = class_name_from_id_with_budget(info.class_id(), budget)?;
        let anchor = anchor_from_path_id(info.path_id(), budget)?;
        let raw = ObjectBytes::copy_from_slice(replacement, budget)?;

        Ok(Self {
            info: info.clone_without_loaded_data(),
            class: UnityClass::with_properties(info.class_id(), class_name, anchor, out.properties),
            byte_order,
            raw,
            payload_provenance: ObjectPayloadProvenance::TypedReplacement,
            typetree_warnings: out.warnings,
            typetree_stats: out.stats,
        })
    }

    fn from_owned_replacement_with_compiled_schema(
        file: &SerializedFile,
        info: &ObjectInfo,
        replacement: Vec<u8>,
        schema: &TypeTreeSchema,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<Self> {
        let byte_order = file.header.byte_order();
        let out = parse_replacement_extent(
            replacement.as_slice(),
            byte_order,
            info.path_id(),
            |reader| schema.read_object(reader, budget, options),
        )?;
        if !out.complete {
            return Err(BinaryObjectReplacementError::Incomplete {
                path_id: info.path_id(),
                consumed: replacement.len(),
            }
            .into());
        }
        let class_name = class_name_from_id_with_budget(info.class_id(), budget)?;
        let anchor = anchor_from_path_id(info.path_id(), budget)?;

        Ok(Self {
            info: info.clone_without_loaded_data(),
            class: UnityClass::with_properties(info.class_id(), class_name, anchor, out.properties),
            byte_order,
            raw: ObjectBytes::Inline(replacement),
            payload_provenance: ObjectPayloadProvenance::TypedReplacement,
            typetree_warnings: out.warnings,
            typetree_stats: out.stats,
        })
    }

    fn from_owned_raw_replacement(
        file: &SerializedFile,
        info: &ObjectInfo,
        replacement: Vec<u8>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self> {
        validate_raw_replacement_len(replacement.len())?;
        let class_name = class_name_from_id_with_budget(info.class_id(), budget)?;
        let anchor = anchor_from_path_id(info.path_id(), budget)?;
        Ok(Self {
            info: info.clone_without_loaded_data(),
            class: UnityClass::new(info.class_id(), class_name, anchor),
            byte_order: file.header.byte_order(),
            raw: ObjectBytes::Inline(replacement),
            payload_provenance: ObjectPayloadProvenance::RawReplacement,
            typetree_warnings: Vec::new(),
            typetree_stats: TypeTreeTraversalStats::default(),
        })
    }

    pub fn path_id(&self) -> i64 {
        self.info.path_id()
    }

    pub fn class_id(&self) -> i32 {
        self.info.class_id()
    }

    pub fn class_name(&self) -> &str {
        &self.class.class_name
    }

    pub fn name(&self) -> Option<String> {
        self.class.get("m_Name").and_then(|v| match v {
            UnityValue::String(s) => Some(s.clone()),
            _ => None,
        })
    }

    pub fn get(&self, key: &str) -> Option<&UnityValue> {
        self.class.get(key)
    }

    pub fn set(&mut self, key: String, value: UnityValue) {
        self.class.set(key, value);
    }

    pub fn has_property(&self, key: &str) -> bool {
        self.class.has_property(key)
    }

    pub fn property_names(&self) -> Vec<&String> {
        self.class.properties().keys().collect()
    }

    pub fn as_unity_class(&self) -> &UnityClass {
        &self.class
    }

    pub fn as_unity_class_mut(&mut self) -> &mut UnityClass {
        &mut self.class
    }

    pub fn as_gameobject(&self) -> Result<GameObject> {
        if self.class_id() != 1 {
            return Err(BinaryError::invalid_data(format!(
                "Object is not a GameObject (class_id: {})",
                self.class_id()
            )));
        }
        GameObject::from_typetree(self.class.properties())
    }

    pub fn as_transform(&self) -> Result<Transform> {
        if self.class_id() != 4 {
            return Err(BinaryError::invalid_data(format!(
                "Object is not a Transform (class_id: {})",
                self.class_id()
            )));
        }
        Transform::from_typetree(self.class.properties())
    }

    pub fn is_gameobject(&self) -> bool {
        self.class_id() == 1
    }

    pub fn is_transform(&self) -> bool {
        self.class_id() == 4
    }

    pub fn describe(&self) -> String {
        let name = self.name().unwrap_or_else(|| "<unnamed>".to_string());
        format!(
            "{} '{}' (ID:{}, PathID:{})",
            self.class_name(),
            name,
            self.class_id(),
            self.path_id()
        )
    }

    /// Returns the current payload, which may differ from the source object-table extent.
    pub fn raw_data(&self) -> &[u8] {
        self.raw.as_slice()
    }

    /// Returns the current payload length independently of source object-table metadata.
    pub fn payload_len(&self) -> usize {
        self.raw.as_slice().len()
    }

    /// Returns how the current payload entered this owned object.
    pub const fn payload_provenance(&self) -> ObjectPayloadProvenance {
        self.payload_provenance
    }

    pub fn typetree_warnings(&self) -> &[TypeTreeParseWarning] {
        &self.typetree_warnings
    }

    pub fn typetree_stats(&self) -> TypeTreeTraversalStats {
        self.typetree_stats
    }

    /// Returns the source object-table byte size, not necessarily [`Self::payload_len`].
    ///
    /// Synthetic objects carry constructor metadata here. Replacement parsing intentionally
    /// preserves the source metadata instead of fabricating an on-disk extent.
    pub fn byte_size(&self) -> u32 {
        self.info.byte_size()
    }

    /// Returns the source object-table offset, which is not changed by replacement parsing.
    ///
    /// Synthetic objects carry their constructor-provided offset here.
    pub fn byte_start(&self) -> u64 {
        self.info.byte_start()
    }

    pub fn byte_order(&self) -> ByteOrder {
        self.byte_order
    }
}

fn class_name_from_id(class_id: i32) -> String {
    unity_asset_core::get_class_name(class_id).unwrap_or_else(|| format!("Class_{}", class_id))
}

fn class_name_from_id_with_budget(class_id: i32, budget: &mut AssetLoadBudget) -> Result<String> {
    match unity_asset_core::get_class_name_str(class_id) {
        Some(name) => copy_string_with_budget(name, budget, "Unity class name"),
        None => {
            const MAX_CLASS_NAME_LEN: usize = "Class_".len() + 11;
            let mut name = string_with_capacity(MAX_CLASS_NAME_LEN, budget, "Unity class name")?;
            write!(&mut name, "Class_{class_id}").map_err(|_| {
                BinaryError::memory_error("Failed to format fallback Unity class name")
            })?;
            Ok(name)
        }
    }
}

fn anchor_from_path_id(path_id: i64, budget: &mut AssetLoadBudget) -> Result<String> {
    const MAX_PATH_ID_LEN: usize = 20;
    let mut anchor = string_with_capacity(MAX_PATH_ID_LEN, budget, "Unity object anchor")?;
    write!(&mut anchor, "{path_id}")
        .map_err(|_| BinaryError::memory_error("Failed to format Unity object anchor"))?;
    Ok(anchor)
}

fn copy_string_with_budget(
    value: &str,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<String> {
    let mut copy = string_with_capacity(value.len(), budget, label)?;
    copy.push_str(value);
    Ok(copy)
}

fn string_with_capacity(
    capacity: usize,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<String> {
    let amount = u64::try_from(capacity)
        .map_err(|_| BinaryError::memory_error(format!("{label} capacity does not fit in u64")))?;
    budget.check_bytes(amount)?;
    let mut value = String::new();
    value.try_reserve_exact(capacity).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {capacity} bytes for {label}: {error}"
        ))
    })?;
    budget.consume_bytes(amount)?;
    Ok(value)
}

enum TypeTreeSource {
    Internal(usize),
    External(Arc<TypeTree>),
}

#[derive(Debug)]
struct SelectedObjectSchema {
    schema: TypeTreeSchema,
    origin: ObjectSchemaOrigin,
}

fn type_tree_for_object(file: &SerializedFile, info: &ObjectInfo) -> Option<TypeTreeSource> {
    fn from_internal<'a>(
        file: &'a SerializedFile,
        info: &ObjectInfo,
    ) -> Option<(usize, &'a SerializedType)> {
        if let Some(index) = info.serialized_type_index() {
            let index = usize::try_from(index).ok()?;
            return file
                .types()
                .get(index)
                .map(|serialized_type| (index, serialized_type));
        }
        file.types()
            .iter()
            .enumerate()
            .find(|(_, serialized_type)| serialized_type.class_id == info.class_id())
    }

    if file.type_tree_enabled()
        && let Some((type_index, typ)) = from_internal(file, info)
        && !typ.type_tree.is_empty()
    {
        return Some(TypeTreeSource::Internal(type_index));
    }

    // Best-effort fallback: stripped files can supply a registry externally.
    // We also allow this fallback even when `enable_type_tree = true` but the internal entry is missing/empty.
    file.type_tree_registry().and_then(|r| {
        if let Some((_, typ)) = from_internal(file, info)
            && typ.is_script_type()
            && typ.script_id != [0u8; 16]
            && let Some(tree) = r.resolve_script(&file.unity_version, typ.class_id, typ.script_id)
        {
            return Some(TypeTreeSource::External(tree));
        }

        r.resolve(&file.unity_version, info.class_id())
            .map(TypeTreeSource::External)
    })
}

fn object_bytes<'a>(file: &'a SerializedFile, info: &'a ObjectInfo) -> Result<&'a [u8]> {
    if let Some(data) = info.loaded_data() {
        return Ok(data);
    }
    file.object_bytes(info)
}

fn owned_object_bytes(
    file: &SerializedFile,
    info: &ObjectInfo,
    budget: &mut AssetLoadBudget,
) -> Result<ObjectBytes> {
    match info.loaded_data() {
        Some(data) => ObjectBytes::copy_from_slice(data, budget),
        None => {
            let (start, end) = object_range(file, info)?;
            let base = file.data_base_offset();
            Ok(ObjectBytes::Shared {
                data: file.data_shared(),
                start: base + start,
                end: base + end,
            })
        }
    }
}

fn object_range(file: &SerializedFile, info: &ObjectInfo) -> Result<(usize, usize)> {
    let start: usize = info.byte_start().try_into().map_err(|_| {
        BinaryError::invalid_data(format!("Object byte_start overflow: {}", info.byte_start()))
    })?;
    let byte_end = info.byte_end()?;
    let end: usize = byte_end
        .try_into()
        .map_err(|_| BinaryError::invalid_data(format!("Object byte_end overflow: {byte_end}")))?;
    if end > file.data().len() {
        return Err(BinaryError::invalid_data(format!(
            "Object data out of bounds (path_id={}, start={}, size={}, file_len={})",
            info.path_id(),
            start,
            info.byte_size(),
            file.data().len()
        )));
    }
    Ok((start, end))
}

fn parse_object_data(
    file: &SerializedFile,
    info: &ObjectInfo,
    byte_order: ByteOrder,
    schema: &TypeTreeSchema,
    budget: &mut AssetLoadBudget,
    options: TypeTreeParseOptions,
) -> Result<TypeTreeParseOutput> {
    let bytes = object_bytes(file, info)?;
    let mut reader = BinaryReader::new(bytes, byte_order);
    schema.read_object(&mut reader, budget, options)
}

fn parse_replacement_extent<T>(
    replacement: &[u8],
    byte_order: ByteOrder,
    path_id: i64,
    parse: impl FnOnce(&mut BinaryReader<'_>) -> Result<T>,
) -> Result<T> {
    let mut reader = BinaryReader::new(replacement, byte_order);
    let output = match parse(&mut reader) {
        Ok(output) => output,
        Err(BinaryError::NotEnoughData { expected, actual }) => {
            return Err(BinaryObjectReplacementError::Truncated {
                path_id,
                required_at_failure: expected,
                available_at_failure: actual,
            }
            .into());
        }
        Err(error) => return Err(error),
    };
    let remaining = reader.remaining();
    if remaining != 0 {
        let consumed = replacement.len().saturating_sub(remaining);
        return Err(BinaryObjectReplacementError::TrailingBytes {
            path_id,
            consumed,
            total: replacement.len(),
            remaining,
        }
        .into());
    }
    Ok(output)
}

fn name_peek_prefix(schema: &TypeTreeSchema) -> Option<(usize, &str)> {
    schema
        .root()
        .children()
        .enumerate()
        .find(|(_, child)| child.name() == "m_Name" || child.name() == "name")
        .map(|(index, child)| (index + 1, child.name()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{ObjectMetadata, ObjectTypeReference, SerializedFileParser, SerializedType};
    use crate::typetree::{TypeTree, TypeTreeNode, TypeTreeRegistry};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use unity_asset_core::{AssetLoadLimits, BudgetError};

    const V22_FIXTURE: &[u8] = include_bytes!(
        "../../unity-asset-write/tests/fixtures/serialized_file_wire/v22.assets.bin"
    );

    fn read(handle: ObjectHandle<'_>) -> UnityObject {
        let mut budget = AssetLoadBudget::default();
        handle.read(&mut budget).unwrap()
    }

    fn replacement_fixture() -> SerializedFile {
        fn node(type_name: &str, name: &str) -> TypeTreeNode {
            TypeTreeNode::with_info(type_name.to_owned(), name.to_owned(), -1)
        }

        let mut target = node("PPtr<Object>", "m_Target");
        target.children = vec![node("int", "m_FileID"), node("long long", "m_PathID")];

        let mut array = node("Array", "Array");
        array.children = vec![node("int", "size"), node("int", "data")];
        let mut values = node("vector", "m_Values");
        values.children.push(array);

        let mut root = node("ReplacementRoot", "ReplacementRoot");
        root.children = vec![node("int", "m_Value"), target, values];
        let mut tree = TypeTree::new();
        tree.add_node(root);

        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        file.types_mut()
            .first_mut()
            .expect("wire fixture has one serialized type")
            .type_tree = tree;
        file
    }

    fn push_i32(bytes: &mut Vec<u8>, value: i32, byte_order: ByteOrder) {
        match byte_order {
            ByteOrder::Big => bytes.extend_from_slice(&value.to_be_bytes()),
            ByteOrder::Little => bytes.extend_from_slice(&value.to_le_bytes()),
        }
    }

    fn push_i64(bytes: &mut Vec<u8>, value: i64, byte_order: ByteOrder) {
        match byte_order {
            ByteOrder::Big => bytes.extend_from_slice(&value.to_be_bytes()),
            ByteOrder::Little => bytes.extend_from_slice(&value.to_le_bytes()),
        }
    }

    fn valid_replacement(file: &SerializedFile) -> Vec<u8> {
        let byte_order = file.header.byte_order();
        let mut bytes = Vec::new();
        push_i32(&mut bytes, 42, byte_order);
        push_i32(&mut bytes, 2, byte_order);
        push_i64(&mut bytes, 73, byte_order);
        push_i32(&mut bytes, 1, byte_order);
        push_i32(&mut bytes, 9, byte_order);
        bytes
    }

    #[test]
    fn replacement_materialization_reuses_identity_schema_and_byte_order() {
        let file = replacement_fixture();
        let handle = file.object_handles().next().unwrap();
        let original_raw = handle.raw_data().unwrap().to_vec();
        let original_identity = (
            handle.path_id(),
            handle.class_id(),
            handle.byte_start(),
            handle.byte_size(),
            handle.info().type_reference(),
            handle.info().metadata(),
        );
        let replacement = valid_replacement(&file);
        let mut budget = AssetLoadBudget::default();

        let materialized = handle
            .materialize_replacement(&replacement, &mut budget)
            .unwrap();
        let selected_root = materialized.schema().unwrap().root();
        let selected_origin = materialized.schema_origin();
        let object = materialized.object();

        assert_eq!(selected_root.type_name(), "ReplacementRoot");
        assert_eq!(selected_origin, handle.schema_origin());
        assert_eq!(object.path_id(), handle.path_id());
        assert_eq!(object.class_id(), handle.class_id());
        assert_eq!(object.byte_order(), file.header.byte_order());
        assert_eq!(object.raw_data(), replacement);
        assert_eq!(object.payload_len(), replacement.len());
        assert_eq!(
            object.payload_provenance(),
            ObjectPayloadProvenance::TypedReplacement
        );
        assert_ne!(
            usize::try_from(object.byte_size()).unwrap(),
            object.payload_len()
        );
        assert_eq!(object.get("m_Value").and_then(UnityValue::as_i64), Some(42));
        assert!(object.typetree_stats().unity_values_materialized > 0);

        assert_eq!(
            (
                handle.path_id(),
                handle.class_id(),
                handle.byte_start(),
                handle.byte_size(),
                handle.info().type_reference(),
                handle.info().metadata(),
            ),
            original_identity
        );
        assert_eq!(handle.raw_data().unwrap(), original_raw);
        assert_eq!(object.info.byte_start(), original_identity.2);
        assert_eq!(object.info.byte_size(), original_identity.3);
        assert!(object.info.loaded_data().is_none());
    }

    #[test]
    fn replacement_reader_retains_one_typed_payload_allocation() {
        let file = replacement_fixture();
        let handle = file.object_handles().next().unwrap();
        let replacement = valid_replacement(&file);
        let mut reader = std::io::Cursor::new(replacement.as_slice());
        let materialized = handle
            .materialize_replacement_from_reader(
                &mut reader,
                replacement.len(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(reader.position(), replacement.len() as u64);
        assert_eq!(materialized.object().raw_data(), replacement);
        assert_eq!(
            materialized.object().payload_provenance(),
            ObjectPayloadProvenance::TypedReplacement
        );
        assert!(materialized.schema().is_some());
    }

    #[test]
    fn raw_replacement_reader_preserves_identity_byte_order_and_exact_budget_growth() {
        fn materialize(payload: &[u8]) -> (UnityObject, u64, usize) {
            let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
            let handle = file.object_handles().next().unwrap();
            let mut reader = std::io::Cursor::new(payload);
            let mut budget = AssetLoadBudget::default();
            let object = handle
                .materialize_raw_replacement_from_reader(&mut reader, payload.len(), &mut budget)
                .unwrap()
                .into_object();
            assert_eq!(reader.position(), payload.len() as u64);
            assert_eq!(object.path_id(), handle.path_id());
            assert_eq!(object.class_id(), handle.class_id());
            assert_eq!(object.byte_order(), file.header.byte_order());
            let retained_capacity = match &object.raw {
                ObjectBytes::Inline(bytes) => bytes.capacity(),
                other => panic!("raw replacement must retain inline bytes, got {other:?}"),
            };
            (object, budget.usage().bytes, retained_capacity)
        }

        let (_, empty_usage, empty_capacity) = materialize(b"");
        let (short, short_usage, short_capacity) = materialize(b"raw!");
        let (long, long_usage, long_capacity) = materialize(b"raw payload!");
        assert_eq!(short.raw_data(), b"raw!");
        assert_eq!(long.raw_data(), b"raw payload!");
        assert!(short.as_unity_class().properties().is_empty());
        assert_eq!(
            short.payload_provenance(),
            ObjectPayloadProvenance::RawReplacement
        );
        assert_eq!(empty_capacity, 0);
        assert_eq!(short_usage - empty_usage, short_capacity as u64);
        assert_eq!(long_usage - empty_usage, long_capacity as u64);

        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let handle = file.object_handles().next().unwrap();
        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: short_usage - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = handle
            .materialize_raw_replacement_from_reader(
                &mut std::io::Cursor::new(b"raw!"),
                4,
                &mut one_short,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded { .. })
        ));
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn oversized_raw_replacement_is_rejected_before_allocation_or_read() {
        struct RejectReads;

        impl Read for RejectReads {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                panic!("oversized raw replacement must be rejected before reading")
            }
        }

        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let handle = file.object_handles().next().unwrap();
        let oversized = usize::try_from(u64::from(u32::MAX) + 1).unwrap();
        let mut reader = RejectReads;
        let mut budget = AssetLoadBudget::default();

        let error = handle
            .materialize_raw_replacement_from_reader(&mut reader, oversized, &mut budget)
            .unwrap_err();

        assert!(matches!(
            error,
            BinaryError::ObjectReplacement(
                BinaryObjectReplacementError::RawPayloadTooLarge { length }
            ) if length == oversized
        ));
        assert_eq!(budget.usage(), Default::default());
    }

    #[test]
    fn replacement_reference_scans_consume_exactly_without_materializing_values() {
        let file = replacement_fixture();
        let handle = file.object_handles().next().unwrap();
        let replacement = valid_replacement(&file);

        let scan = handle
            .scan_replacement_pptrs(&replacement, &mut AssetLoadBudget::default())
            .unwrap()
            .unwrap();
        assert!(scan.internal.is_empty());
        assert_eq!(scan.external, vec![(2, 73)]);
        assert_eq!(scan.stats.wire_bytes, replacement.len() as u64);
        assert_eq!(scan.stats.unity_values_materialized, 0);

        let occurrences = handle
            .scan_replacement_reference_occurrences(&replacement, &mut AssetLoadBudget::default())
            .unwrap()
            .unwrap();
        assert_eq!(occurrences.occurrences.len(), 1);
        assert_eq!(occurrences.occurrences[0].file_id, 2);
        assert_eq!(occurrences.occurrences[0].path_id, 73);
        assert_eq!(occurrences.stats.wire_bytes, replacement.len() as u64);
        assert_eq!(occurrences.stats.unity_values_materialized, 0);
    }

    #[test]
    fn replacement_extent_rejects_typed_trailing_and_truncated_payloads() {
        let file = replacement_fixture();
        let handle = file.object_handles().next().unwrap();
        let replacement = valid_replacement(&file);
        let mut trailing = replacement.clone();
        trailing.push(0xFF);

        let read_error = handle
            .read_replacement(&trailing, &mut AssetLoadBudget::default())
            .unwrap_err();
        assert!(matches!(
            read_error,
            BinaryError::ObjectReplacement(BinaryObjectReplacementError::TrailingBytes {
                path_id,
                consumed,
                total,
                remaining: 1,
            }) if path_id == handle.path_id()
                && consumed == replacement.len()
                && total == trailing.len()
        ));

        let scan_error = handle
            .scan_replacement_pptrs(&trailing, &mut AssetLoadBudget::default())
            .unwrap_err();
        assert!(matches!(
            scan_error,
            BinaryError::ObjectReplacement(BinaryObjectReplacementError::TrailingBytes {
                remaining: 1,
                ..
            })
        ));

        let truncated = &replacement[..replacement.len() - 1];
        let truncated_error = handle
            .read_replacement(truncated, &mut AssetLoadBudget::default())
            .unwrap_err();
        assert!(truncated_error.is_recoverable());
        assert_eq!(
            truncated_error.severity(),
            crate::error::ErrorSeverity::Medium
        );
        assert!(matches!(
            truncated_error,
            BinaryError::ObjectReplacement(BinaryObjectReplacementError::Truncated {
                path_id,
                required_at_failure: 4,
                available_at_failure: 3,
            }) if path_id == handle.path_id()
        ));

        for scan_error in [
            handle
                .scan_replacement_pptrs(truncated, &mut AssetLoadBudget::default())
                .unwrap_err(),
            handle
                .scan_replacement_reference_occurrences(truncated, &mut AssetLoadBudget::default())
                .unwrap_err(),
        ] {
            assert!(matches!(
                scan_error,
                BinaryError::ObjectReplacement(BinaryObjectReplacementError::Truncated {
                    path_id,
                    required_at_failure: 4,
                    available_at_failure: 3,
                }) if path_id == handle.path_id()
            ));
        }
    }

    #[test]
    fn lenient_replacement_rejects_incomplete_sequence_at_exact_eof() {
        let file = replacement_fixture();
        let handle = file.object_handles().next().unwrap();
        let byte_order = file.header.byte_order();
        let mut incomplete = valid_replacement(&file)[..16].to_vec();
        push_i32(&mut incomplete, 1, byte_order);

        let error = handle
            .read_replacement_with_options(
                &incomplete,
                &mut AssetLoadBudget::default(),
                TypeTreeParseOptions {
                    mode: TypeTreeParseMode::Lenient,
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::ObjectReplacement(BinaryObjectReplacementError::Incomplete {
                path_id,
                consumed,
            }) if path_id == handle.path_id() && consumed == incomplete.len()
        ));
    }

    #[test]
    fn replacement_extent_rejects_invalid_sequence_shape() {
        let file = replacement_fixture();
        let handle = file.object_handles().next().unwrap();
        let byte_order = file.header.byte_order();
        let mut invalid = Vec::new();
        push_i32(&mut invalid, 42, byte_order);
        push_i32(&mut invalid, 2, byte_order);
        push_i64(&mut invalid, 73, byte_order);
        push_i32(&mut invalid, -1, byte_order);

        let error = handle
            .read_replacement(&invalid, &mut AssetLoadBudget::default())
            .unwrap_err();
        assert!(matches!(error, BinaryError::InvalidData(_)));
    }

    #[test]
    fn replacement_scan_propagates_structured_budget_failure() {
        let file = replacement_fixture();
        let handle = file.object_handles().next().unwrap();
        let replacement = valid_replacement(&file);
        handle
            .schema(&mut AssetLoadBudget::default())
            .unwrap()
            .expect("warm the immutable internal schema cache");
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = handle
            .scan_replacement_pptrs(&replacement, &mut budget)
            .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "entries",
                limit: 1,
                requested: 2,
            })
        ));
        assert_eq!(budget.usage().entries, 1);
    }

    #[test]
    fn replacement_raw_copy_checks_the_last_payload_byte_before_allocation() {
        let file = replacement_fixture();
        let handle = file.object_handles().next().unwrap();
        let replacement = valid_replacement(&file);
        handle
            .schema(&mut AssetLoadBudget::default())
            .unwrap()
            .expect("warm the immutable internal schema cache");

        let mut baseline = AssetLoadBudget::default();
        handle
            .read_replacement(&replacement, &mut baseline)
            .unwrap();
        let total = baseline.usage().bytes;
        let payload_bytes = u64::try_from(replacement.len()).unwrap();
        let before_copy = total.checked_sub(payload_bytes).unwrap();
        let one_short = total.checked_sub(1).unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: one_short,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = handle
            .read_replacement(&replacement, &mut budget)
            .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == one_short && requested == total
        ));
        assert_eq!(budget.usage().bytes, before_copy);
    }

    #[test]
    fn replacement_read_requires_a_canonical_schema() {
        let mut file = replacement_fixture();
        file.set_type_tree_enabled(false);
        let handle = file.object_handles().next().unwrap();

        let error = handle
            .read_replacement(&[], &mut AssetLoadBudget::default())
            .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::ObjectReplacement(BinaryObjectReplacementError::MissingSchema {
                path_id,
                class_id,
            }) if path_id == handle.path_id() && class_id == handle.class_id()
        ));
        assert!(
            handle
                .scan_replacement_pptrs(&[0xFF], &mut AssetLoadBudget::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn parser_preload_state_drives_handles_and_owned_objects() {
        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        // This wire fixture intentionally uses a scalar root to exercise SerializedFile state,
        // not object materialization. Keep this test on the raw-object path.
        file.set_type_tree_enabled(false);
        let path_id = file.objects()[0].path_id();
        let original = file
            .find_object_handle(path_id)
            .unwrap()
            .raw_data()
            .unwrap()
            .to_vec();
        assert!(!original.is_empty());

        let committed = read(file.find_object_handle(path_id).unwrap());
        assert_eq!(committed.raw_data(), original);
        assert_eq!(committed.payload_len(), original.len());
        assert_eq!(
            committed.payload_provenance(),
            ObjectPayloadProvenance::Committed
        );

        let mut preloaded =
            SerializedFileParser::from_bytes_with_options(V22_FIXTURE.to_vec(), true).unwrap();
        preloaded.set_type_tree_enabled(false);
        let handle = preloaded.find_object_handle(path_id).unwrap();
        assert_eq!(handle.raw_data().unwrap(), original);
        let loaded = read(handle);
        assert_eq!(loaded.raw_data(), original);
        assert_eq!(loaded.payload_len(), original.len());
        assert_eq!(loaded.payload_provenance(), ObjectPayloadProvenance::Loaded);
    }

    #[test]
    fn raw_objects_expose_bytes_without_synthetic_properties() {
        let object = UnityObject::from_raw(1, 7, vec![0xAA, 0xBB]).unwrap();

        assert_eq!(object.raw_data(), [0xAA, 0xBB]);
        assert_eq!(object.payload_len(), 2);
        assert_eq!(
            object.payload_provenance(),
            ObjectPayloadProvenance::Synthetic
        );
        assert!(object.class.properties().is_empty());
        assert_eq!(object.typetree_stats(), TypeTreeTraversalStats::default());
    }

    #[test]
    fn owned_payload_copy_checks_then_charges_the_budget() {
        let limits = AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        };
        let mut short = AssetLoadBudget::new(limits).unwrap();
        let error = ObjectBytes::copy_from_slice(&[1, 2], &mut short).unwrap_err();
        assert!(matches!(error, BinaryError::Budget(_)));
        assert_eq!(short.usage().bytes, 0);

        let limits = AssetLoadLimits {
            max_bytes: 2,
            ..AssetLoadLimits::default()
        };
        let mut exact = AssetLoadBudget::new(limits).unwrap();
        let copied = ObjectBytes::copy_from_slice(&[1, 2], &mut exact).unwrap();
        assert_eq!(copied.as_slice(), [1, 2]);
        assert_eq!(exact.usage().bytes, 2);
    }

    #[test]
    fn object_handles_share_one_budgeted_canonical_schema() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let info = &file.objects()[0];
        let first_handle = ObjectHandle::new(&file, info);
        let second_handle = ObjectHandle::new(&file, info);
        let mut budget = AssetLoadBudget::default();

        let first = first_handle
            .schema(&mut budget)
            .unwrap()
            .expect("fixture object should have an internal TypeTree");
        let after_first = budget.usage();
        let second = second_handle
            .schema(&mut budget)
            .unwrap()
            .expect("fixture object should reuse its internal TypeTree schema");

        assert_eq!(first.root(), second.root());
        assert_eq!(budget.usage(), after_first);
    }

    #[test]
    fn ordinary_schema_ignores_an_unrelated_malformed_managed_catalog() {
        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let mut malformed_tree = TypeTree::new();
        malformed_tree.add_node(TypeTreeNode::with_info(
            "BrokenManagedType".to_owned(),
            "BrokenManagedType".to_owned(),
            -1,
        ));
        let mut malformed = SerializedType::new(114);
        malformed.type_tree = malformed_tree;
        *file.ref_types_mut() = vec![malformed];

        let handle = file.object_handles().next().unwrap();
        let schema = handle.schema(&mut AssetLoadBudget::default()).unwrap();

        assert!(schema.is_some());
    }

    #[test]
    fn serialized_type_indices_bind_cache_keys_to_file_owned_trees() {
        fn node(type_name: &str, name: &str) -> TypeTreeNode {
            TypeTreeNode::with_info(type_name.to_owned(), name.to_owned(), -1)
        }

        fn object_tree(root_name: &str) -> TypeTree {
            let mut managed_type = node("ReferencedObjectType", "type");
            managed_type.children = vec![
                node("string", "class"),
                node("string", "ns"),
                node("string", "asm"),
            ];
            let mut referenced = node("ReferencedObject", "m_Reference");
            referenced.children = vec![managed_type, node("ReferencedObjectData", "data")];
            let mut root = node(root_name, root_name);
            root.children.push(referenced);
            let mut tree = TypeTree::new();
            tree.add_node(root);
            tree
        }

        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let mut first_type = SerializedType::new(1);
        first_type.type_tree = object_tree("FirstRoot");
        let mut second_type = SerializedType::new(1);
        second_type.type_tree = object_tree("SecondRoot");
        *file.types_mut() = vec![first_type, second_type];

        let mut referenced_root = node("ManagedRoot", "ManagedRoot");
        referenced_root.children.push(node("int", "m_Value"));
        let mut referenced_tree = TypeTree::new();
        referenced_tree.add_node(referenced_root);
        let mut referenced_type = SerializedType::new(114);
        referenced_type.class_name = "ManagedRoot".to_owned();
        referenced_type.namespace = "Tests".to_owned();
        referenced_type.assembly_name = "Tests".to_owned();
        referenced_type.type_tree = referenced_tree;
        *file.ref_types_mut() = vec![referenced_type];

        let first_info = ObjectInfo::from_wire(
            1,
            0,
            0,
            ObjectTypeReference::SerializedTypeIndex { index: 0 },
            1,
            Some(0),
            ObjectMetadata::None,
        );
        let second_info = ObjectInfo::from_wire(
            2,
            0,
            0,
            ObjectTypeReference::SerializedTypeIndex { index: 1 },
            1,
            Some(1),
            ObjectMetadata::None,
        );
        let mut budget = AssetLoadBudget::default();

        assert!(matches!(
            type_tree_for_object(&file, &first_info),
            Some(TypeTreeSource::Internal(0))
        ));
        assert!(matches!(
            type_tree_for_object(&file, &second_info),
            Some(TypeTreeSource::Internal(1))
        ));

        let first_schema = ObjectHandle::new(&file, &first_info)
            .schema(&mut budget)
            .unwrap()
            .unwrap();
        let after_first = budget.usage().entries;
        let second_schema = ObjectHandle::new(&file, &second_info)
            .schema(&mut budget)
            .unwrap()
            .unwrap();

        assert_eq!(first_schema.root().type_name(), "FirstRoot");
        assert_eq!(second_schema.root().type_name(), "SecondRoot");
        assert_ne!(first_schema.root(), second_schema.root());
        assert_eq!(budget.usage().entries - after_first, 7);

        let after_both = budget.usage();
        assert_eq!(
            ObjectHandle::new(&file, &first_info)
                .schema(&mut budget)
                .unwrap()
                .unwrap()
                .root()
                .type_name(),
            "FirstRoot"
        );
        assert_eq!(
            ObjectHandle::new(&file, &second_info)
                .schema(&mut budget)
                .unwrap()
                .unwrap()
                .root()
                .type_name(),
            "SecondRoot"
        );
        assert_eq!(budget.usage(), after_both);
    }

    #[test]
    fn mutating_the_type_table_invalidates_cached_object_schemas() {
        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let first = file
            .object_handles()
            .next()
            .unwrap()
            .schema(&mut AssetLoadBudget::default())
            .unwrap()
            .unwrap();

        let mut replacement_root = TypeTreeNode::with_info(
            "ReplacementRoot".to_owned(),
            "ReplacementRoot".to_owned(),
            -1,
        );
        replacement_root.children.push(TypeTreeNode::with_info(
            "int".to_owned(),
            "m_Value".to_owned(),
            -1,
        ));
        let mut replacement = TypeTree::new();
        replacement.add_node(replacement_root);
        file.types_mut()[0].type_tree = replacement;

        let second = file
            .object_handles()
            .next()
            .unwrap()
            .schema(&mut AssetLoadBudget::default())
            .unwrap()
            .unwrap();
        assert_eq!(second.root().type_name(), "ReplacementRoot");
        assert_ne!(first.root(), second.root());
    }

    #[test]
    fn mutating_ref_types_invalidates_cached_managed_catalogs() {
        fn node(type_name: &str, name: &str) -> TypeTreeNode {
            TypeTreeNode::with_info(type_name.to_owned(), name.to_owned(), -1)
        }

        fn managed_tree(root_name: &str) -> TypeTree {
            let mut root = node(root_name, root_name);
            root.children.push(node("int", "m_Value"));
            let mut tree = TypeTree::new();
            tree.add_node(root);
            tree
        }

        let mut managed_type = node("ReferencedObjectType", "type");
        managed_type.children = vec![
            node("string", "class"),
            node("string", "ns"),
            node("string", "asm"),
        ];
        let mut referenced = node("ReferencedObject", "m_Reference");
        referenced.children = vec![managed_type, node("ReferencedObjectData", "data")];
        let mut object_root = node("ObjectRoot", "ObjectRoot");
        object_root.children.push(referenced);
        let mut object_tree = TypeTree::new();
        object_tree.add_node(object_root);

        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        file.types_mut()[0].type_tree = object_tree;
        let mut ref_type = SerializedType::new(114);
        ref_type.class_name = "Managed".to_owned();
        ref_type.namespace = "Tests".to_owned();
        ref_type.assembly_name = "Tests".to_owned();
        ref_type.type_tree = managed_tree("FirstManaged");
        *file.ref_types_mut() = vec![ref_type];

        let first = file
            .object_handles()
            .next()
            .unwrap()
            .schema(&mut AssetLoadBudget::default())
            .unwrap()
            .unwrap();
        assert_eq!(
            first
                .resolve_managed_root("Managed", "Tests", "Tests")
                .unwrap()
                .type_name(),
            "FirstManaged"
        );

        file.ref_types_mut()[0].type_tree = managed_tree("SecondManaged");
        let second = file
            .object_handles()
            .next()
            .unwrap()
            .schema(&mut AssetLoadBudget::default())
            .unwrap()
            .unwrap();
        assert_eq!(
            second
                .resolve_managed_root("Managed", "Tests", "Tests")
                .unwrap()
                .type_name(),
            "SecondManaged"
        );
        assert_ne!(first.root(), second.root());
    }

    #[test]
    fn schema_and_catalog_resource_failures_can_be_retried() {
        fn node(type_name: &str, name: &str) -> TypeTreeNode {
            TypeTreeNode::with_info(type_name.to_owned(), name.to_owned(), -1)
        }

        let mut managed_type = node("ReferencedObjectType", "type");
        managed_type.children = vec![
            node("string", "class"),
            node("string", "ns"),
            node("string", "asm"),
        ];
        let mut referenced = node("ReferencedObject", "m_Reference");
        referenced.children = vec![managed_type, node("ReferencedObjectData", "data")];
        let mut root = node("Root", "Root");
        root.children.push(referenced);
        let mut object_tree = TypeTree::new();
        object_tree.add_node(root);

        let mut ref_root = node("Managed", "Managed");
        ref_root.children.push(node("int", "m_Value"));
        let mut ref_tree = TypeTree::new();
        ref_tree.add_node(ref_root);

        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let mut object_type = SerializedType::new(1);
        object_type.type_tree = object_tree;
        *file.types_mut() = vec![object_type];
        let mut ref_type = SerializedType::new(114);
        ref_type.class_name = "Managed".to_owned();
        ref_type.namespace = "Tests".to_owned();
        ref_type.assembly_name = "Tests".to_owned();
        ref_type.type_tree = ref_tree;
        *file.ref_types_mut() = vec![ref_type];
        let info = ObjectInfo::for_standalone_class(1, 0, 0, 1).unwrap();
        let handle = ObjectHandle::new(&file, &info);

        let mut constrained = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 7,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            handle.schema(&mut constrained),
            Err(BinaryError::Budget(_))
        ));

        let schema = handle
            .schema(&mut AssetLoadBudget::default())
            .unwrap()
            .expect("a larger budget should retry both schema and catalog compilation");
        assert_eq!(schema.root().type_name(), "Root");
    }

    #[test]
    fn malformed_managed_catalog_failure_is_negative_cached() {
        fn node(type_name: &str, name: &str) -> TypeTreeNode {
            TypeTreeNode::with_info(type_name.to_owned(), name.to_owned(), -1)
        }

        let mut managed_type = node("ReferencedObjectType", "type");
        managed_type.children = vec![
            node("string", "class"),
            node("string", "ns"),
            node("string", "asm"),
        ];
        let mut referenced = node("ReferencedObject", "m_Reference");
        referenced.children = vec![managed_type, node("ReferencedObjectData", "data")];
        let mut object_root = node("Root", "Root");
        object_root.children.push(referenced);
        let mut object_tree = TypeTree::new();
        object_tree.add_node(object_root);

        let mut managed_tree = TypeTree::new();
        managed_tree.add_node(node("Managed", "Managed"));

        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        file.types_mut()[0].type_tree = object_tree;
        let mut malformed = SerializedType::new(114);
        malformed.type_tree = managed_tree;
        *file.ref_types_mut() = vec![malformed];

        let handle = file.object_handles().next().unwrap();
        let mut budget = AssetLoadBudget::default();
        let first = handle.schema(&mut budget).unwrap_err();
        assert!(first.to_string().contains("no class name"));
        let after_first = budget.usage();

        let second = handle.schema(&mut budget).unwrap_err();
        assert!(second.to_string().contains("no class name"));
        assert_eq!(budget.usage(), after_first);
    }

    #[derive(Debug)]
    struct ChangingRegistry {
        calls: AtomicUsize,
    }

    impl TypeTreeRegistry for ChangingRegistry {
        fn resolve(&self, _unity_version: &str, _class_id: i32) -> Option<Arc<TypeTree>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let name = if call == 0 {
                "FirstExternal"
            } else {
                "SecondExternal"
            };
            let mut root = TypeTreeNode::with_info(name.to_owned(), name.to_owned(), -1);
            root.children.push(TypeTreeNode::with_info(
                "int".to_owned(),
                "m_Value".to_owned(),
                -1,
            ));
            let mut tree = TypeTree::new();
            tree.add_node(root);
            Some(Arc::new(tree))
        }
    }

    #[test]
    fn replacement_materialization_uses_the_exact_external_registry_schema() {
        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        file.set_type_tree_enabled(false);
        let registry = Arc::new(ChangingRegistry {
            calls: AtomicUsize::new(0),
        });
        file.set_type_tree_registry(Some(registry.clone()));
        let handle = file.object_handles().next().unwrap();
        let mut replacement = Vec::new();
        push_i32(&mut replacement, 77, file.header.byte_order());

        let materialized = handle
            .materialize_replacement(&replacement, &mut AssetLoadBudget::default())
            .unwrap();

        assert_eq!(
            materialized.schema_origin(),
            Some(ObjectSchemaOrigin::ExternalRegistry)
        );
        assert_eq!(
            materialized.schema().unwrap().root().type_name(),
            "FirstExternal"
        );
        assert_eq!(
            materialized
                .object()
                .get("m_Value")
                .and_then(UnityValue::as_i64),
            Some(77)
        );
        assert_eq!(materialized.object().raw_data(), replacement);
        assert_eq!(
            materialized.object().payload_provenance(),
            ObjectPayloadProvenance::TypedReplacement
        );
        assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn external_registry_results_are_not_cached_without_a_stable_identity() {
        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        file.set_type_tree_enabled(false);
        let registry = Arc::new(ChangingRegistry {
            calls: AtomicUsize::new(0),
        });
        file.set_type_tree_registry(Some(registry.clone()));
        let handle = file.object_handles().next().unwrap();
        let mut budget = AssetLoadBudget::default();

        let first = handle.schema(&mut budget).unwrap().unwrap();
        let second = handle.schema(&mut budget).unwrap().unwrap();

        assert_eq!(first.root().type_name(), "FirstExternal");
        assert_eq!(second.root().type_name(), "SecondExternal");
        assert_ne!(first.root(), second.root());
        assert_eq!(registry.calls.load(Ordering::SeqCst), 2);
    }
}
