use std::collections::TryReserveError;

use thiserror::Error;
use unity_asset_binary::BinaryError;
use unity_asset_binary::asset::{FileIdentifier, SerializedFile};
use unity_asset_core::{
    AllocationSizeError, AssetLoadBudget, BudgetError, DigestV1, vec_allocation_bytes,
};

use super::external_table::ExternalTableMutation;
use crate::object::EncodedSerializedObject;

const OBJECT_EDIT_ALLOCATION_RESOURCE: &str = "serialized_file_object_edits";

/// Typed failures produced while retaining a SerializedFile object replacement.
#[derive(Debug, Error)]
pub enum SerializedFileEditError {
    #[error("failed to reserve {requested} SerializedFile object edits: {source}")]
    Allocation {
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error(transparent)]
    AllocationSize(#[from] AllocationSizeError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("SerializedFile object edit arithmetic overflow for {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("SerializedFile already contains an encoded edit for object {path_id}")]
    DuplicateObjectEdit { path_id: i64 },
    #[error("SerializedFile has no object with path ID {path_id}")]
    ObjectNotFound { path_id: i64 },
    #[error(
        "SerializedFile object {path_id} class changed from encoded class {expected} to {actual}"
    )]
    ObjectClassMismatch {
        path_id: i64,
        expected: i32,
        actual: i32,
    },
    #[error("failed to read original bytes for SerializedFile object {path_id}: {source}")]
    ReadOriginal {
        path_id: i64,
        #[source]
        source: BinaryError,
    },
    #[error("SerializedFile object {path_id} original digest changed from {expected} to {actual}")]
    OriginalDigestMismatch {
        path_id: i64,
        expected: DigestV1,
        actual: DigestV1,
    },
}

#[derive(Debug)]
struct ObjectBytesEdit {
    path_id: i64,
    class_id: i32,
    original_digest: DigestV1,
    bytes: Vec<u8>,
}

/// Deterministic, budgeted edits to apply when rebuilding one `SerializedFile`.
///
/// Object replacements are retained in ascending path-ID order. The representation is private so
/// callers cannot bypass allocation accounting or create duplicate replacements for one object.
#[derive(Debug, Default)]
pub struct SerializedFileEdits {
    object_bytes: Vec<ObjectBytesEdit>,
    pub(crate) external_table: Option<ExternalTableMutation>,
}

impl SerializedFileEdits {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            object_bytes: Vec::new(),
            external_table: None,
        }
    }

    /// Retains one fully encoded object while atomically charging retained storage.
    ///
    /// The encoded capability binds bytes to their source object, class, and original digest.
    /// Duplicate objects are rejected because one encoder must aggregate all ordered operations.
    pub fn try_insert_encoded_object(
        &mut self,
        encoded: EncodedSerializedObject,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), SerializedFileEditError> {
        let (path_id, class_id, original_digest, bytes) = encoded.into_file_edit_parts();
        let byte_allocation = vec_allocation_bytes::<u8>(bytes.capacity())?;
        match self
            .object_bytes
            .binary_search_by_key(&path_id, |edit| edit.path_id)
        {
            Ok(_) => Err(SerializedFileEditError::DuplicateObjectEdit { path_id }),
            Err(index) => self.insert_object_bytes(
                index,
                ObjectBytesEdit {
                    path_id,
                    class_id,
                    original_digest,
                    bytes,
                },
                byte_allocation,
                budget,
            ),
        }
    }

    /// Returns a retained replacement for `path_id`.
    #[must_use]
    pub(crate) fn object_bytes(&self, path_id: i64) -> Option<&[u8]> {
        self.object_bytes
            .binary_search_by_key(&path_id, |edit| edit.path_id)
            .ok()
            .map(|index| self.object_bytes[index].bytes.as_slice())
    }

    /// Returns validated external identifiers appended by an external-table allocator.
    #[must_use]
    pub fn external_additions(&self) -> &[FileIdentifier] {
        self.external_table
            .as_ref()
            .map_or(&[], ExternalTableMutation::additions)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.object_bytes.is_empty() && self.external_additions().is_empty()
    }

    pub(crate) fn validate_for(
        &self,
        file: &SerializedFile,
    ) -> Result<(), SerializedFileEditError> {
        for edit in &self.object_bytes {
            let handle = file.find_object_handle(edit.path_id).ok_or(
                SerializedFileEditError::ObjectNotFound {
                    path_id: edit.path_id,
                },
            )?;
            let actual_class = handle.class_id();
            if actual_class != edit.class_id {
                return Err(SerializedFileEditError::ObjectClassMismatch {
                    path_id: edit.path_id,
                    expected: edit.class_id,
                    actual: actual_class,
                });
            }
            let original =
                handle
                    .raw_data()
                    .map_err(|source| SerializedFileEditError::ReadOriginal {
                        path_id: edit.path_id,
                        source,
                    })?;
            let actual = DigestV1::hash_bytes(original);
            if actual != edit.original_digest {
                return Err(SerializedFileEditError::OriginalDigestMismatch {
                    path_id: edit.path_id,
                    expected: edit.original_digest,
                    actual,
                });
            }
        }
        Ok(())
    }

    fn insert_object_bytes(
        &mut self,
        index: usize,
        edit: ObjectBytesEdit,
        byte_allocation: u64,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), SerializedFileEditError> {
        budget.check_entries(1)?;
        if self.object_bytes.len() < self.object_bytes.capacity() {
            budget.check_bytes(byte_allocation)?;
            budget.consume_entries(1)?;
            budget.consume_bytes(byte_allocation)?;
            self.object_bytes.insert(index, edit);
            return Ok(());
        }

        let required = self.object_bytes.len().checked_add(1).ok_or(
            SerializedFileEditError::ArithmeticOverflow {
                resource: "object edit count",
            },
        )?;
        let planned_table = vec_allocation_bytes::<ObjectBytesEdit>(required)?;
        let planned = byte_allocation.checked_add(planned_table).ok_or(
            SerializedFileEditError::ArithmeticOverflow {
                resource: OBJECT_EDIT_ALLOCATION_RESOURCE,
            },
        )?;
        budget.check_bytes(planned)?;

        let mut staged = Vec::new();
        staged.try_reserve_exact(required).map_err(|source| {
            SerializedFileEditError::Allocation {
                requested: required,
                source,
            }
        })?;
        let actual_table = vec_allocation_bytes::<ObjectBytesEdit>(staged.capacity())?;
        let actual = byte_allocation.checked_add(actual_table).ok_or(
            SerializedFileEditError::ArithmeticOverflow {
                resource: OBJECT_EDIT_ALLOCATION_RESOURCE,
            },
        )?;
        budget.check_bytes(actual)?;
        budget.consume_entries(1)?;
        budget.consume_bytes(actual)?;

        staged.append(&mut self.object_bytes);
        staged.insert(index, edit);
        self.object_bytes = staged;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use unity_asset_binary::asset::SerializedFileParser;
    use unity_asset_core::{AssetLoadLimits, AssetLoadUsage};

    use super::*;
    use crate::object::{
        SerializedObjectEncoder, UnsafeRawObjectAcknowledgement, UnsafeRawObjectReplacement,
    };

    const V8_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v8.assets.bin");
    const V21_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v21.assets.bin");
    const V22_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v22.assets.bin");

    fn unsafe_encoded(
        file: &SerializedFile,
        path_id: i64,
        bytes: Vec<u8>,
    ) -> EncodedSerializedObject {
        let original = file
            .find_object_handle(path_id)
            .expect("fixture object")
            .raw_data()
            .expect("fixture object bytes");
        SerializedObjectEncoder::new(file, path_id)
            .expect("bind fixture object encoder")
            .encode_unsafe_raw(
                UnsafeRawObjectReplacement::new(
                    DigestV1::hash_bytes(original),
                    bytes,
                    UnsafeRawObjectAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
                ),
                &mut AssetLoadBudget::default(),
            )
            .expect("encode fixture replacement")
    }

    #[test]
    fn encoded_replacement_retains_the_original_byte_allocation() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let path_id = file.objects()[0].path_id();
        let mut replacement = Vec::with_capacity(32);
        replacement.extend_from_slice(&[1, 2, 3]);
        let allocation = replacement.as_ptr();
        let encoded = unsafe_encoded(&file, path_id, replacement);
        let mut edits = SerializedFileEdits::new();
        edits
            .try_insert_encoded_object(encoded, &mut AssetLoadBudget::default())
            .unwrap();

        let retained = edits.object_bytes(path_id).unwrap();
        assert_eq!(retained.as_ptr(), allocation);
        assert_eq!(retained.len(), 3);
        assert_eq!(retained, [1, 2, 3]);
    }

    #[test]
    fn failed_insert_leaves_edits_and_budget_unchanged() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let path_id = file.objects()[0].path_id();
        let mut measured = AssetLoadBudget::default();
        let mut measured_edits = SerializedFileEdits::new();
        measured_edits
            .try_insert_encoded_object(unsafe_encoded(&file, path_id, vec![1, 2, 3]), &mut measured)
            .unwrap();
        let required = measured.usage().bytes;
        assert!(required > 0);

        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: required - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let mut edits = SerializedFileEdits::new();
        let error = edits
            .try_insert_encoded_object(unsafe_encoded(&file, path_id, vec![1, 2, 3]), &mut budget)
            .unwrap_err();

        assert!(matches!(
            error,
            SerializedFileEditError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert!(edits.is_empty());
        assert_eq!(budget.usage(), AssetLoadUsage::default());
    }

    #[test]
    fn duplicate_edit_is_rejected_without_charging_budget_or_replacing_bytes() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let path_id = file.objects()[0].path_id();
        let mut edits = SerializedFileEdits::new();
        edits
            .try_insert_encoded_object(
                unsafe_encoded(&file, path_id, vec![1]),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let mut budget = AssetLoadBudget::default();
        let error = edits
            .try_insert_encoded_object(unsafe_encoded(&file, path_id, vec![2]), &mut budget)
            .unwrap_err();

        assert!(matches!(
            error,
            SerializedFileEditError::DuplicateObjectEdit { path_id: actual }
                if actual == path_id
        ));
        assert_eq!(edits.object_bytes.len(), 1);
        assert_eq!(edits.object_bytes(path_id), Some([1].as_slice()));
        assert_eq!(budget.usage(), AssetLoadUsage::default());
    }

    #[test]
    fn encoded_edit_is_bound_to_its_source_object_identity() {
        let v8 = SerializedFileParser::from_bytes(V8_FIXTURE.to_vec()).unwrap();
        let v21 = SerializedFileParser::from_bytes(V21_FIXTURE.to_vec()).unwrap();
        let v22 = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();

        let mut unknown = SerializedFileEdits::new();
        let v8_path = v8.objects()[0].path_id();
        unknown
            .try_insert_encoded_object(
                unsafe_encoded(&v8, v8_path, vec![8]),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert!(matches!(
            unknown.validate_for(&v22),
            Err(SerializedFileEditError::ObjectNotFound { path_id }) if path_id == v8_path
        ));

        let mut stale = SerializedFileEdits::new();
        let shared_path = v21.objects()[0].path_id();
        assert_eq!(shared_path, v22.objects()[0].path_id());
        stale
            .try_insert_encoded_object(
                unsafe_encoded(&v21, shared_path, vec![21]),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert!(matches!(
            stale.validate_for(&v22),
            Err(SerializedFileEditError::OriginalDigestMismatch { path_id, .. })
                if path_id == shared_path
        ));
    }

    #[test]
    fn class_mismatch_is_rejected_before_original_bytes_are_read() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let object = &file.objects()[0];
        let original = file.object_bytes(object).unwrap();
        let edits = SerializedFileEdits {
            object_bytes: vec![ObjectBytesEdit {
                path_id: object.path_id(),
                class_id: object.class_id() + 1,
                original_digest: DigestV1::hash_bytes(original),
                bytes: vec![1],
            }],
            external_table: None,
        };

        assert!(matches!(
            edits.validate_for(&file),
            Err(SerializedFileEditError::ObjectClassMismatch {
                path_id,
                expected,
                actual,
            }) if path_id == object.path_id()
                && expected == object.class_id() + 1
                && actual == object.class_id()
        ));
    }
}
