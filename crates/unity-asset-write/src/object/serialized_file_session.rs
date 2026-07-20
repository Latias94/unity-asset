use indexmap::IndexMap;
use unity_asset_binary::asset::{ObjectInfo, SerializedFile};
use unity_asset_binary::object::ObjectHandle;
use unity_asset_binary::reader::ByteOrder;
use unity_asset_core::{AssetLoadBudget, UnityAssetError, UnityClass, UnityValue};

use crate::serialized_file::SerializedFileEdits;
use crate::typetree::rewrite_object;
use crate::{ChangeTracker, Endian, Result};

/// A UnityPy-like edit session for a single `SerializedFile`.
///
/// The session stores overridden object raw bytes (by `path_id`) and tracks a "changed" flag.
/// The actual file rebuild is performed by `SerializedFileWriter::save(file, session.edits())`.
#[derive(Debug)]
pub struct SerializedFileEditSession<'a> {
    file: &'a SerializedFile,
    edits: SerializedFileEdits,
    changed: bool,
}

impl<'a> SerializedFileEditSession<'a> {
    pub fn new(file: &'a SerializedFile) -> Self {
        Self {
            file,
            edits: SerializedFileEdits::default(),
            changed: false,
        }
    }

    pub fn file(&self) -> &'a SerializedFile {
        self.file
    }

    pub fn edits(&self) -> &SerializedFileEdits {
        &self.edits
    }

    pub fn edits_mut(&mut self) -> &mut SerializedFileEdits {
        &mut self.edits
    }

    pub fn into_edits(self) -> SerializedFileEdits {
        self.edits
    }

    /// A convenience wrapper that loads the object, applies a mutation, and stores the re-encoded bytes.
    ///
    /// This requires a valid TypeTree for the object. If TypeTree is stripped and no external registry
    /// is available, this returns an error.
    pub fn edit_object(
        &mut self,
        path_id: i64,
        budget: &mut AssetLoadBudget,
        f: impl FnOnce(&mut UnityClass) -> Result<()>,
    ) -> Result<()> {
        let handle = self.file.find_object_handle(path_id).ok_or_else(|| {
            UnityAssetError::format(format!(
                "Object not found in SerializedFile: path_id={}",
                path_id
            ))
        })?;

        let mut obj = handle.read(budget).map_err(|e| {
            UnityAssetError::with_source(
                format!("Failed to parse object for edit: path_id={}", path_id),
                e,
            )
        })?;

        f(&mut obj.class)?;

        let bytes =
            encode_object_typetree(self.file, handle.info(), obj.class.properties(), budget)?;
        self.edits
            .try_set_object_bytes(path_id, bytes, budget)
            .map_err(|error| {
                UnityAssetError::with_source(
                    format!("Failed to retain object edit: path_id={path_id}"),
                    error,
                )
            })?;
        self.mark_changed();
        Ok(())
    }

    /// Encode and store overridden object bytes for an object, using its TypeTree.
    pub fn save_typetree(
        &mut self,
        path_id: i64,
        properties: &IndexMap<String, UnityValue>,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        let info = self.file.find_object(path_id).ok_or_else(|| {
            UnityAssetError::format(format!(
                "Object not found in SerializedFile: path_id={}",
                path_id
            ))
        })?;
        let bytes = encode_object_typetree(self.file, info, properties, budget)?;
        self.edits
            .try_set_object_bytes(path_id, bytes, budget)
            .map_err(|error| {
                UnityAssetError::with_source(
                    format!("Failed to retain object edit: path_id={path_id}"),
                    error,
                )
            })?;
        self.mark_changed();
        Ok(())
    }

    /// Store overridden bytes without running TypeTree encoding (escape hatch).
    pub fn set_raw_data(
        &mut self,
        path_id: i64,
        bytes: Vec<u8>,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        self.edits
            .try_set_object_bytes(path_id, bytes, budget)
            .map_err(|error| {
                UnityAssetError::with_source(
                    format!("Failed to retain raw object edit: path_id={path_id}"),
                    error,
                )
            })?;
        self.mark_changed();
        Ok(())
    }
}

impl ChangeTracker for SerializedFileEditSession<'_> {
    fn mark_changed(&mut self) {
        self.changed = true;
    }

    fn is_changed(&self) -> bool {
        self.changed
    }

    fn clear_changed(&mut self) {
        self.changed = false;
    }
}

fn encode_object_typetree(
    file: &SerializedFile,
    info: &ObjectInfo,
    properties: &IndexMap<String, UnityValue>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>> {
    let endian = match file.header.byte_order() {
        ByteOrder::Big => Endian::Big,
        ByteOrder::Little => Endian::Little,
    };
    let schema = ObjectHandle::new(file, info)
        .schema(budget)
        .map_err(|error| {
            UnityAssetError::with_source(
                format!(
                    "Failed to compile TypeTree for object write: path_id={} class_id={}",
                    info.path_id(),
                    info.class_id()
                ),
                error,
            )
        })?;
    let Some(schema) = schema else {
        return Err(UnityAssetError::format(format!(
            "TypeTree is unavailable for object write: path_id={} class_id={}",
            info.path_id(),
            info.class_id()
        )));
    };

    let original = file.object_bytes(info).map_err(|e| {
        UnityAssetError::with_source(
            format!(
                "Failed to read original object bytes for TypeTree write: path_id={} class_id={}",
                info.path_id(),
                info.class_id()
            ),
            e,
        )
    })?;

    let (bytes, _stats) = rewrite_object(&schema, properties, original, endian, budget)?;
    Ok(bytes)
}
