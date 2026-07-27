//! Staged logical-reference lowering for mutation preflight.

use indexmap::IndexMap;
use thiserror::Error;
use unity_asset_binary::typetree::{IntegerSignedness, PrimitiveKind};
use unity_asset_core::{
    AllocationSizeError, AssetLoadBudget, BudgetError, DigestBuildError, DigestV1, FieldPath,
    ObjectKind, RevisionedObjectHandle, SemanticDigestError, UnityValue, UnityValueCloneError,
    UnityValueKind, ValuePathError, field_schema_digest, index_map_allocation_bytes,
    semantic_value_digest, string_allocation_bytes, vec_allocation_bytes, yaml_field_schema_digest,
};
use unity_asset_write::object::{
    SerializedFieldGuard, SerializedManagedReferenceLayout, SerializedManagedReferenceType,
    SerializedObjectCandidate, SerializedObjectEncodeError, SerializedObjectMutation,
    SerializedPPtrLayout, SerializedValueSchema, SerializedValueSchemaError,
};
use unity_asset_write::serialized_file::{ExternalTableAllocator, ExternalTableError};

use crate::reference::encoding::{
    ReferenceDestination, ReferenceDestinationEncoder, ReferenceEncodingError,
    ReferenceEncodingHint,
};

use super::super::plan::MutationValueOwned;
use super::super::{
    FieldGuard, MutationField, MutationValue, MutationValueRef, ReferenceTarget, WorkspaceView,
};
use super::yaml::{YamlCandidateError, YamlObjectCandidate, YamlSemanticOperation};

const YAML_FILE_ID: &str = "fileID";
const YAML_GUID: &str = "guid";
const YAML_TYPE: &str = "type";
const YAML_MEMBER_FILE_ID: &str = "m_FileID";
const YAML_MEMBER_GUID: &str = "m_GUID";
const YAML_MEMBER_TYPE: &str = "m_Type";

/// Revision-bound codec shared by every operation in one prepare run.
pub(super) struct StagedReferenceMutationCodec<'view> {
    view: &'view dyn WorkspaceView,
    destinations: ReferenceDestinationEncoder,
}

impl<'view> StagedReferenceMutationCodec<'view> {
    pub(super) fn build(
        view: &'view dyn WorkspaceView,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ReferenceMutationCodecError> {
        Ok(Self {
            view,
            destinations: ReferenceDestinationEncoder::build(view, budget)?,
        })
    }

    /// Verifies a binary reference against the current staged candidate and applies its replacement.
    pub(super) fn apply_binary_reference_replace(
        &self,
        owner: &RevisionedObjectHandle,
        candidate: &mut SerializedObjectCandidate<'_>,
        externals: &mut ExternalTableAllocator<'_>,
        ordinal: u32,
        path: FieldPath,
        schema_digest: DigestV1,
        expected: &ReferenceTarget,
        replacement: &ReferenceTarget,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), ReferenceMutationCodecError> {
        self.validate_binary_owner(owner, candidate)?;
        let actual_schema = field_schema_digest(candidate.schema_digest(), &path)?;
        if actual_schema != schema_digest {
            return Err(ReferenceMutationCodecError::FieldSchemaMismatch {
                expected: schema_digest,
                actual: actual_schema,
            });
        }

        let current = candidate.value_at_path(&path)?;
        let schema = candidate.value_schema_at_path(&path)?;
        let layout =
            schema
                .pptr_layout()
                .ok_or(ReferenceMutationCodecError::BinarySchemaIsNotPPtr {
                    actual: schema.kind(),
                })?;
        let raw = BinaryPPtr::read(current, layout)?;
        let external = binary_external_identifier(raw, externals)?;
        if !self.destinations.binary_current_matches(
            self.view,
            owner,
            expected,
            raw.file_id,
            raw.path_id,
            external,
            budget,
        )? {
            return Err(ReferenceMutationCodecError::ExpectedReferenceMismatch {
                actual_file_id: raw.file_id,
                actual_path_id: raw.path_id,
            });
        }

        let guard =
            SerializedFieldGuard::from_observed(candidate.schema_digest(), &path, current, budget)?;
        let hint = ReferenceEncodingHint::binary(external.map(|identifier| identifier.type_));
        let destination = self
            .destinations
            .encode(self.view, owner, replacement, hint, budget)?;
        let (file_id, path_id) = binary_destination_ids(destination, externals, budget)?;
        let replacement = binary_pptr_value(current, layout, file_id, path_id, budget)?;

        candidate.apply(
            SerializedObjectMutation::replace_field(ordinal, path, guard, replacement),
            budget,
        )?;
        Ok(())
    }

    /// Verifies a YAML reference against the current staged candidate and applies its replacement.
    pub(super) fn apply_yaml_reference_replace(
        &self,
        owner: &RevisionedObjectHandle,
        candidate: &mut YamlObjectCandidate,
        ordinal: u32,
        path: &FieldPath,
        schema_digest: DigestV1,
        expected: &ReferenceTarget,
        replacement: &ReferenceTarget,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), ReferenceMutationCodecError> {
        self.validate_yaml_owner(owner, candidate)?;
        let current = candidate.class().value_at_path(path)?;
        let raw = YamlPPtr::read(current)?;
        let actual_schema = yaml_field_schema_digest(candidate.class(), path, current, budget)?;
        if actual_schema != schema_digest {
            return Err(ReferenceMutationCodecError::FieldSchemaMismatch {
                expected: schema_digest,
                actual: actual_schema,
            });
        }

        let hint = ReferenceEncodingHint::yaml(raw.type_id);
        if !self.destinations.yaml_current_matches(
            self.view,
            owner,
            expected,
            raw.file_id,
            raw.guid,
            budget,
        )? {
            return Err(ReferenceMutationCodecError::ExpectedReferenceMismatch {
                actual_file_id: yaml_file_id_for_diagnostic(raw.file_id),
                actual_path_id: raw.file_id,
            });
        }

        let guard = FieldGuard::new(actual_schema, semantic_value_digest(current, budget)?);
        let destination = self
            .destinations
            .encode(self.view, owner, replacement, hint, budget)?;
        let replacement = yaml_pptr_value(destination, Some(raw), budget)?;
        candidate.apply(
            YamlSemanticOperation::FieldReplace {
                ordinal,
                path,
                guard,
                replacement,
            },
            budget,
        )?;
        Ok(())
    }

    /// Recursively lowers an owned mutation value through a binary replacement schema.
    pub(super) fn lower_binary_mutation_value(
        &self,
        owner: &RevisionedObjectHandle,
        schema: SerializedValueSchema<'_>,
        current: Option<&UnityValue>,
        value: MutationValue,
        externals: &mut ExternalTableAllocator<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<UnityValue, ReferenceMutationCodecError> {
        if owner.object().kind() != ObjectKind::Binary {
            return Err(ReferenceMutationCodecError::OwnerKindMismatch {
                expected: ObjectKind::Binary,
                actual: owner.object().kind(),
            });
        }
        self.lower_binary_owned(
            owner,
            schema,
            current,
            value.into_owned(),
            externals,
            budget,
            0,
        )
    }

    /// Recursively lowers an owned mutation value to YAML, resolving every nested reference.
    pub(super) fn lower_yaml_mutation_value(
        &self,
        owner: &RevisionedObjectHandle,
        current: Option<&UnityValue>,
        value: MutationValue,
        budget: &mut AssetLoadBudget,
    ) -> Result<UnityValue, ReferenceMutationCodecError> {
        if owner.object().kind() != ObjectKind::Yaml {
            return Err(ReferenceMutationCodecError::OwnerKindMismatch {
                expected: ObjectKind::Yaml,
                actual: owner.object().kind(),
            });
        }
        self.lower_yaml_owned(owner, current, value.into_owned(), budget, 0)
    }

    fn lower_binary_owned(
        &self,
        owner: &RevisionedObjectHandle,
        schema: SerializedValueSchema<'_>,
        current: Option<&UnityValue>,
        value: MutationValueOwned,
        externals: &mut ExternalTableAllocator<'_>,
        budget: &mut AssetLoadBudget,
        depth: u32,
    ) -> Result<UnityValue, ReferenceMutationCodecError> {
        budget.observe_depth(depth)?;
        Ok(match value {
            MutationValueOwned::Null => UnityValue::Null,
            MutationValueOwned::Bool(value) => UnityValue::Bool(value),
            MutationValueOwned::Signed(value) => UnityValue::Integer(value),
            MutationValueOwned::Unsigned(value) => UnityValue::Unsigned(value),
            MutationValueOwned::Float64(value) => UnityValue::Float(value.to_f64()),
            MutationValueOwned::String(value) => UnityValue::String(retain_string(value, budget)?),
            MutationValueOwned::Bytes(value) => {
                UnityValue::Bytes(retain_bytes(value.into_vec(), budget)?)
            }
            MutationValueOwned::Reference(target) => {
                let layout = schema.pptr_layout().ok_or(
                    ReferenceMutationCodecError::BinarySchemaIsNotPPtr {
                        actual: schema.kind(),
                    },
                )?;
                let current_raw = current
                    .map(|value| BinaryPPtr::read(value, layout))
                    .transpose()?;
                let hint = current_raw.map_or(Ok(ReferenceEncodingHint::binary(None)), |raw| {
                    binary_hint(raw, externals)
                })?;
                let destination = self
                    .destinations
                    .encode(self.view, owner, &target, hint, budget)?;
                let (file_id, path_id) = binary_destination_ids(destination, externals, budget)?;
                binary_pptr_value_optional(current, layout, file_id, path_id, budget)?
            }
            MutationValueOwned::Array(values) => {
                let current_values = current.and_then(|value| match value {
                    UnityValue::Array(values) => Some(values.as_slice()),
                    _ => None,
                });
                let child_depth = child_depth(depth)?;
                let mut output = allocate_array(values.len(), budget)?;
                for (index, value) in values.into_iter().enumerate() {
                    let child_schema = schema.element(index).ok_or(
                        ReferenceMutationCodecError::BinarySchemaCannotDescendIndex { index },
                    )?;
                    let current = current_values.and_then(|values| values.get(index));
                    output.push(self.lower_binary_owned(
                        owner,
                        child_schema,
                        current,
                        value.into_owned(),
                        externals,
                        budget,
                        child_depth,
                    )?);
                }
                UnityValue::Array(output)
            }
            MutationValueOwned::Object(fields) => {
                let current_fields = current.and_then(|value| match value {
                    UnityValue::Object(fields) => Some(fields),
                    _ => None,
                });
                let managed_layout = schema.managed_reference_layout();
                let managed_payload_schema = managed_layout.and_then(|layout| {
                    managed_reference_type_from_mutation(layout, &fields).and_then(|runtime_type| {
                        schema.field_for_managed_type(layout.payload_field(), runtime_type)
                    })
                });
                let child_depth = child_depth(depth)?;
                let mut output = allocate_object(fields.len(), budget)?;
                for field in fields {
                    let (name, value) = field.into_parts();
                    let child_schema = match managed_layout {
                        Some(layout) if name == layout.payload_field() => managed_payload_schema,
                        _ => current
                            .and_then(|current| schema.field_for_value(&name, current))
                            .or_else(|| schema.field(&name)),
                    }
                    .ok_or(ReferenceMutationCodecError::BinarySchemaCannotDescendField)?;
                    let current = current_fields.and_then(|fields| fields.get(&name));
                    let value = self.lower_binary_owned(
                        owner,
                        child_schema,
                        current,
                        value.into_owned(),
                        externals,
                        budget,
                        child_depth,
                    )?;
                    output.insert(retain_string(name, budget)?, value);
                }
                UnityValue::Object(output)
            }
        })
    }

    fn lower_yaml_owned(
        &self,
        owner: &RevisionedObjectHandle,
        current: Option<&UnityValue>,
        value: MutationValueOwned,
        budget: &mut AssetLoadBudget,
        depth: u32,
    ) -> Result<UnityValue, ReferenceMutationCodecError> {
        budget.observe_depth(depth)?;
        Ok(match value {
            MutationValueOwned::Null => UnityValue::Null,
            MutationValueOwned::Bool(value) => UnityValue::Bool(value),
            MutationValueOwned::Signed(value) => UnityValue::Integer(value),
            MutationValueOwned::Unsigned(value) => UnityValue::Unsigned(value),
            MutationValueOwned::Float64(value) => UnityValue::Float(value.to_f64()),
            MutationValueOwned::String(value) => UnityValue::String(retain_string(value, budget)?),
            MutationValueOwned::Bytes(value) => {
                UnityValue::Bytes(retain_bytes(value.into_vec(), budget)?)
            }
            MutationValueOwned::Reference(target) => {
                let template = current.and_then(|value| YamlPPtr::read(value).ok());
                let hint = ReferenceEncodingHint::yaml(template.and_then(|value| value.type_id));
                let destination = self
                    .destinations
                    .encode(self.view, owner, &target, hint, budget)?;
                yaml_pptr_value(destination, template, budget)?
            }
            MutationValueOwned::Array(values) => {
                let current_values = current.and_then(|value| match value {
                    UnityValue::Array(values) => Some(values.as_slice()),
                    _ => None,
                });
                let child_depth = child_depth(depth)?;
                let mut output = allocate_array(values.len(), budget)?;
                for (index, value) in values.into_iter().enumerate() {
                    output.push(self.lower_yaml_owned(
                        owner,
                        current_values.and_then(|values| values.get(index)),
                        value.into_owned(),
                        budget,
                        child_depth,
                    )?);
                }
                UnityValue::Array(output)
            }
            MutationValueOwned::Object(fields) => {
                let current_fields = current.and_then(|value| match value {
                    UnityValue::Object(fields) => Some(fields),
                    _ => None,
                });
                let child_depth = child_depth(depth)?;
                let mut output = allocate_object(fields.len(), budget)?;
                for field in fields {
                    let (name, value) = field.into_parts();
                    let current = current_fields.and_then(|fields| fields.get(&name));
                    let value = self.lower_yaml_owned(
                        owner,
                        current,
                        value.into_owned(),
                        budget,
                        child_depth,
                    )?;
                    output.insert(retain_string(name, budget)?, value);
                }
                UnityValue::Object(output)
            }
        })
    }

    fn validate_binary_owner(
        &self,
        owner: &RevisionedObjectHandle,
        candidate: &SerializedObjectCandidate<'_>,
    ) -> Result<(), ReferenceMutationCodecError> {
        if owner.object().kind() != ObjectKind::Binary {
            return Err(ReferenceMutationCodecError::OwnerKindMismatch {
                expected: ObjectKind::Binary,
                actual: owner.object().kind(),
            });
        }
        if owner.object().binary_path_id() != Some(candidate.path_id()) {
            return Err(ReferenceMutationCodecError::CandidateOwnerMismatch);
        }
        Ok(())
    }

    fn validate_yaml_owner(
        &self,
        owner: &RevisionedObjectHandle,
        candidate: &YamlObjectCandidate,
    ) -> Result<(), ReferenceMutationCodecError> {
        if owner.object().kind() != ObjectKind::Yaml {
            return Err(ReferenceMutationCodecError::OwnerKindMismatch {
                expected: ObjectKind::Yaml,
                actual: owner.object().kind(),
            });
        }
        if owner.object() != candidate.object() {
            return Err(ReferenceMutationCodecError::CandidateOwnerMismatch);
        }
        Ok(())
    }
}

fn managed_reference_type_from_mutation<'value>(
    layout: SerializedManagedReferenceLayout<'_>,
    fields: &'value [MutationField],
) -> Option<SerializedManagedReferenceType<'value>> {
    let type_fields = mutation_object_field(fields, layout.type_field())?;
    Some(SerializedManagedReferenceType::new(
        mutation_string_field(type_fields, layout.class_field())?,
        mutation_string_field(type_fields, layout.namespace_field())?,
        mutation_string_field(type_fields, layout.assembly_field())?,
    ))
}

fn mutation_object_field<'value>(
    fields: &'value [MutationField],
    name: &str,
) -> Option<&'value [MutationField]> {
    let field = fields.iter().find(|field| field.name() == name)?;
    match field.value().view() {
        MutationValueRef::Object(fields) => Some(fields),
        _ => None,
    }
}

fn mutation_string_field<'value>(
    fields: &'value [MutationField],
    name: &str,
) -> Option<&'value str> {
    let field = fields.iter().find(|field| field.name() == name)?;
    match field.value().view() {
        MutationValueRef::String(value) => Some(value),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
struct BinaryPPtr {
    file_id: i32,
    path_id: i64,
}

impl BinaryPPtr {
    fn read(
        value: &UnityValue,
        layout: SerializedPPtrLayout<'_>,
    ) -> Result<Self, ReferenceMutationCodecError> {
        let UnityValue::Object(fields) = value else {
            return Err(ReferenceMutationCodecError::ReferenceValueKind {
                actual: value.kind(),
            });
        };
        let file_id = fields
            .get(layout.file_field())
            .and_then(UnityValue::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .ok_or(ReferenceMutationCodecError::InvalidBinaryFileId)?;
        let path_id = fields
            .get(layout.path_field())
            .and_then(UnityValue::as_i64)
            .ok_or(ReferenceMutationCodecError::InvalidBinaryPathId)?;
        Ok(Self { file_id, path_id })
    }
}

fn binary_hint(
    raw: BinaryPPtr,
    externals: &ExternalTableAllocator<'_>,
) -> Result<ReferenceEncodingHint, ReferenceMutationCodecError> {
    Ok(ReferenceEncodingHint::binary(
        binary_external_identifier(raw, externals)?.map(|identifier| identifier.type_),
    ))
}

fn binary_external_identifier<'table>(
    raw: BinaryPPtr,
    externals: &'table ExternalTableAllocator<'_>,
) -> Result<Option<&'table unity_asset_binary::asset::FileIdentifier>, ReferenceMutationCodecError>
{
    if raw.file_id <= 0 {
        return Ok(None);
    }
    match externals.identifier(raw.file_id) {
        Some(identifier) => Ok(Some(identifier)),
        None if raw.path_id == 0 => Ok(None),
        None => Err(ReferenceMutationCodecError::BinaryExternalFileIdMissing {
            file_id: raw.file_id,
        }),
    }
}

fn binary_destination_ids(
    destination: ReferenceDestination,
    externals: &mut ExternalTableAllocator<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<(i32, i64), ReferenceMutationCodecError> {
    match destination {
        ReferenceDestination::Null => Ok((0, 0)),
        ReferenceDestination::BinaryLocal { path_id } => Ok((0, path_id)),
        ReferenceDestination::BinaryExternal {
            path_id,
            identifier,
        } => Ok((externals.intern(identifier, budget)?, path_id)),
        ReferenceDestination::YamlLocal { .. } | ReferenceDestination::YamlExternal { .. } => {
            Err(ReferenceMutationCodecError::DestinationFormatMismatch)
        }
    }
}

fn binary_pptr_value(
    current: &UnityValue,
    layout: SerializedPPtrLayout<'_>,
    file_id: i32,
    path_id: i64,
    budget: &mut AssetLoadBudget,
) -> Result<UnityValue, ReferenceMutationCodecError> {
    binary_pptr_value_optional(Some(current), layout, file_id, path_id, budget)
}

fn binary_pptr_value_optional(
    current: Option<&UnityValue>,
    layout: SerializedPPtrLayout<'_>,
    file_id: i32,
    path_id: i64,
    budget: &mut AssetLoadBudget,
) -> Result<UnityValue, ReferenceMutationCodecError> {
    let mut fields = match current {
        Some(current) => {
            let cloned = current.try_clone_with_budget(budget)?;
            let UnityValue::Object(fields) = cloned else {
                return Err(ReferenceMutationCodecError::ReferenceValueKind {
                    actual: current.kind(),
                });
            };
            fields
        }
        None => {
            if layout.field_count() != 2 {
                return Err(ReferenceMutationCodecError::BinaryPPtrTemplateRequired {
                    fields: layout.field_count(),
                });
            }
            let mut fields = allocate_object(2, budget)?;
            let file = binary_integer(file_id.into(), layout.file_primitive(), "file ID")?;
            let path = binary_integer(path_id, layout.path_primitive(), "path ID")?;
            if layout.file_index() < layout.path_index() {
                fields.insert(clone_string(layout.file_field(), budget)?, file);
                fields.insert(clone_string(layout.path_field(), budget)?, path);
            } else {
                fields.insert(clone_string(layout.path_field(), budget)?, path);
                fields.insert(clone_string(layout.file_field(), budget)?, file);
            }
            return Ok(UnityValue::Object(fields));
        }
    };

    let file = binary_integer(file_id.into(), layout.file_primitive(), "file ID")?;
    let path = binary_integer(path_id, layout.path_primitive(), "path ID")?;
    *fields
        .get_mut(layout.file_field())
        .ok_or(ReferenceMutationCodecError::InvalidBinaryFileId)? = file;
    *fields
        .get_mut(layout.path_field())
        .ok_or(ReferenceMutationCodecError::InvalidBinaryPathId)? = path;
    Ok(UnityValue::Object(fields))
}

fn binary_integer(
    value: i64,
    primitive: PrimitiveKind,
    role: &'static str,
) -> Result<UnityValue, ReferenceMutationCodecError> {
    let width = u32::from(primitive.width()) * 8;
    match primitive.signedness() {
        Some(IntegerSignedness::Signed) => {
            let fits = width == 64 || {
                let limit = 1_i128 << (width - 1);
                let value = i128::from(value);
                (-limit..limit).contains(&value)
            };
            if !fits {
                return Err(ReferenceMutationCodecError::BinaryIntegerOutOfRange {
                    role,
                    value,
                    primitive,
                });
            }
            Ok(UnityValue::Integer(value))
        }
        Some(IntegerSignedness::Unsigned) => {
            let value = u64::try_from(value).map_err(|_| {
                ReferenceMutationCodecError::BinaryIntegerOutOfRange {
                    role,
                    value,
                    primitive,
                }
            })?;
            let fits = width == 64 || u128::from(value) < (1_u128 << width);
            if !fits {
                return Err(ReferenceMutationCodecError::BinaryIntegerOutOfRange {
                    role,
                    value: i64::try_from(value).unwrap_or(i64::MAX),
                    primitive,
                });
            }
            Ok(UnityValue::Unsigned(value))
        }
        None => Err(ReferenceMutationCodecError::BinaryIntegerOutOfRange {
            role,
            value,
            primitive,
        }),
    }
}

#[derive(Debug, Clone, Copy)]
struct YamlPPtr<'value> {
    file_field: &'value str,
    guid_field: Option<&'value str>,
    type_field: Option<&'value str>,
    file_id: i64,
    guid: Option<[u8; 16]>,
    type_id: Option<i64>,
}

impl<'value> YamlPPtr<'value> {
    fn read(value: &'value UnityValue) -> Result<Self, ReferenceMutationCodecError> {
        let UnityValue::Object(fields) = value else {
            return Err(ReferenceMutationCodecError::ReferenceValueKind {
                actual: value.kind(),
            });
        };
        let mut file = None;
        let mut guid = None;
        let mut type_id = None;
        for (name, value) in fields {
            match name.as_str() {
                YAML_FILE_ID | YAML_MEMBER_FILE_ID if file.is_none() => {
                    file = Some((name.as_str(), value));
                }
                YAML_GUID | YAML_MEMBER_GUID if guid.is_none() => {
                    guid = Some((name.as_str(), value));
                }
                YAML_TYPE | YAML_MEMBER_TYPE if type_id.is_none() => {
                    type_id = Some((name.as_str(), value));
                }
                _ => return Err(ReferenceMutationCodecError::InvalidYamlReferenceShape),
            }
        }
        let (file_field, file_value) =
            file.ok_or(ReferenceMutationCodecError::InvalidYamlReferenceShape)?;
        let file_id = file_value
            .as_i64()
            .ok_or(ReferenceMutationCodecError::InvalidYamlReferenceShape)?;
        let (guid_field, guid) = match guid {
            Some((field, value)) => {
                let value = value
                    .as_str()
                    .ok_or(ReferenceMutationCodecError::InvalidYamlReferenceShape)?;
                (Some(field), Some(parse_guid(value)?))
            }
            None => (None, None),
        };
        let (type_field, type_id) = match type_id {
            Some((field, value)) => (
                Some(field),
                Some(
                    value
                        .as_i64()
                        .ok_or(ReferenceMutationCodecError::InvalidYamlReferenceShape)?,
                ),
            ),
            None => (None, None),
        };
        if guid.is_some() != type_id.is_some() {
            return Err(ReferenceMutationCodecError::InvalidYamlReferenceShape);
        }
        Ok(Self {
            file_field,
            guid_field,
            type_field,
            file_id,
            guid,
            type_id,
        })
    }

    fn external_field_names(self) -> (&'value str, &'value str) {
        match (self.guid_field, self.type_field) {
            (Some(guid), Some(type_id)) => (guid, type_id),
            _ if self.file_field == YAML_MEMBER_FILE_ID => (YAML_MEMBER_GUID, YAML_MEMBER_TYPE),
            _ => (YAML_GUID, YAML_TYPE),
        }
    }
}

fn yaml_pptr_value(
    destination: ReferenceDestination,
    template: Option<YamlPPtr<'_>>,
    budget: &mut AssetLoadBudget,
) -> Result<UnityValue, ReferenceMutationCodecError> {
    let file_field = template.map_or(YAML_FILE_ID, |value| value.file_field);
    let (file_id, external) = match destination {
        ReferenceDestination::Null => (0, None),
        ReferenceDestination::YamlLocal { file_id } => (file_id, None),
        ReferenceDestination::YamlExternal {
            file_id,
            guid,
            type_id,
        } => (file_id, Some((guid, type_id))),
        ReferenceDestination::BinaryLocal { .. } | ReferenceDestination::BinaryExternal { .. } => {
            return Err(ReferenceMutationCodecError::DestinationFormatMismatch);
        }
    };
    let field_count = if external.is_some() { 3 } else { 1 };
    let mut fields = allocate_object(field_count, budget)?;
    fields.insert(
        clone_string(file_field, budget)?,
        UnityValue::Integer(file_id),
    );
    if let Some((guid, type_id)) = external {
        let (guid_field, type_field) = template
            .map(YamlPPtr::external_field_names)
            .unwrap_or((YAML_GUID, YAML_TYPE));
        fields.insert(
            clone_string(guid_field, budget)?,
            UnityValue::String(format_guid(guid, budget)?),
        );
        fields.insert(
            clone_string(type_field, budget)?,
            UnityValue::Integer(type_id),
        );
    }
    Ok(UnityValue::Object(fields))
}

fn parse_guid(value: &str) -> Result<[u8; 16], ReferenceMutationCodecError> {
    if value.len() != 32 {
        return Err(ReferenceMutationCodecError::InvalidYamlReferenceShape);
    }
    let mut guid = [0_u8; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high =
            hex_value(pair[0]).ok_or(ReferenceMutationCodecError::InvalidYamlReferenceShape)?;
        let low =
            hex_value(pair[1]).ok_or(ReferenceMutationCodecError::InvalidYamlReferenceShape)?;
        guid[index] = (high << 4) | low;
    }
    Ok(guid)
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn format_guid(
    guid: [u8; 16],
    budget: &mut AssetLoadBudget,
) -> Result<String, ReferenceMutationCodecError> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = allocate_string(32, budget, "YAML reference GUID")?;
    for byte in guid {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    Ok(output)
}

fn yaml_file_id_for_diagnostic(value: i64) -> i32 {
    i32::try_from(value).unwrap_or_else(|_| {
        if value.is_negative() {
            i32::MIN
        } else {
            i32::MAX
        }
    })
}

fn allocate_array(
    capacity: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<UnityValue>, ReferenceMutationCodecError> {
    let members = usize_to_u64(capacity, "mutation value array members")?;
    let planned = allocation_bytes(vec_allocation_bytes::<UnityValue>(capacity))?;
    budget.check_members(members)?;
    budget.check_bytes(planned)?;
    let mut values = Vec::new();
    values.try_reserve_exact(capacity).map_err(|_| {
        ReferenceMutationCodecError::AllocationFailed {
            resource: "mutation value array",
            requested: capacity,
        }
    })?;
    let actual = allocation_bytes(vec_allocation_bytes::<UnityValue>(values.capacity()))?;
    budget.check_bytes(actual)?;
    budget.consume_members(members)?;
    budget.consume_bytes(actual)?;
    Ok(values)
}

fn allocate_object(
    capacity: usize,
    budget: &mut AssetLoadBudget,
) -> Result<IndexMap<String, UnityValue>, ReferenceMutationCodecError> {
    let members = usize_to_u64(capacity, "mutation value object members")?;
    let planned = allocation_bytes(index_map_allocation_bytes::<String, UnityValue>(capacity))?;
    budget.check_members(members)?;
    budget.check_bytes(planned)?;
    let mut fields = IndexMap::new();
    fields.try_reserve_exact(capacity).map_err(|_| {
        ReferenceMutationCodecError::AllocationFailed {
            resource: "mutation value object",
            requested: capacity,
        }
    })?;
    let actual = allocation_bytes(index_map_allocation_bytes::<String, UnityValue>(
        fields.capacity(),
    ))?;
    budget.check_bytes(actual)?;
    budget.consume_members(members)?;
    budget.consume_bytes(actual)?;
    Ok(fields)
}

fn clone_string(
    value: &str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ReferenceMutationCodecError> {
    let mut output = allocate_string(value.len(), budget, "reference field name")?;
    output.push_str(value);
    Ok(output)
}

fn allocate_string(
    capacity: usize,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<String, ReferenceMutationCodecError> {
    let planned = allocation_bytes(string_allocation_bytes(capacity))?;
    budget.check_bytes(planned)?;
    let mut output = String::new();
    output.try_reserve_exact(capacity).map_err(|_| {
        ReferenceMutationCodecError::AllocationFailed {
            resource,
            requested: capacity,
        }
    })?;
    let actual = allocation_bytes(string_allocation_bytes(output.capacity()))?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    Ok(output)
}

fn retain_string(
    value: String,
    budget: &mut AssetLoadBudget,
) -> Result<String, ReferenceMutationCodecError> {
    budget.consume_bytes(allocation_bytes(string_allocation_bytes(value.capacity()))?)?;
    Ok(value)
}

fn retain_bytes(
    value: Vec<u8>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, ReferenceMutationCodecError> {
    budget.consume_bytes(allocation_bytes(vec_allocation_bytes::<u8>(
        value.capacity(),
    ))?)?;
    Ok(value)
}

fn allocation_bytes(
    result: Result<u64, AllocationSizeError>,
) -> Result<u64, ReferenceMutationCodecError> {
    result.map_err(|_| ReferenceMutationCodecError::ArithmeticOverflow {
        resource: "mutation value allocation",
    })
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, ReferenceMutationCodecError> {
    u64::try_from(value).map_err(|_| ReferenceMutationCodecError::ArithmeticOverflow { resource })
}

fn child_depth(depth: u32) -> Result<u32, ReferenceMutationCodecError> {
    depth
        .checked_add(1)
        .ok_or(ReferenceMutationCodecError::ArithmeticOverflow {
            resource: "mutation value depth",
        })
}

#[derive(Debug, Error)]
pub(super) enum ReferenceMutationCodecError {
    #[error(transparent)]
    Encoding(Box<ReferenceEncodingError>),
    #[error(transparent)]
    Binary(#[from] SerializedObjectEncodeError),
    #[error(transparent)]
    BinarySchema(#[from] SerializedValueSchemaError),
    #[error(transparent)]
    ExternalTable(#[from] ExternalTableError),
    #[error(transparent)]
    YamlCandidate(#[from] YamlCandidateError),
    #[error(transparent)]
    ValuePath(#[from] ValuePathError),
    #[error(transparent)]
    Digest(#[from] DigestBuildError),
    #[error(transparent)]
    SemanticDigest(#[from] SemanticDigestError),
    #[error(transparent)]
    Clone(#[from] UnityValueCloneError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("reference owner kind is {actual:?}, expected {expected:?}")]
    OwnerKindMismatch {
        expected: ObjectKind,
        actual: ObjectKind,
    },
    #[error("staged candidate does not belong to the declared reference owner")]
    CandidateOwnerMismatch,
    #[error("reference field schema guard failed")]
    FieldSchemaMismatch {
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error("binary reference schema is {actual:?}, expected PPtr")]
    BinarySchemaIsNotPPtr {
        actual: unity_asset_binary::typetree::SemanticKind,
    },
    #[error("binary replacement schema cannot resolve array index {index}")]
    BinarySchemaCannotDescendIndex { index: usize },
    #[error("binary replacement schema cannot resolve a named object field")]
    BinarySchemaCannotDescendField,
    #[error("reference value has kind {actual}, expected object")]
    ReferenceValueKind { actual: UnityValueKind },
    #[error("binary PPtr file ID is missing or invalid")]
    InvalidBinaryFileId,
    #[error("binary PPtr path ID is missing or invalid")]
    InvalidBinaryPathId,
    #[error("binary PPtr external file ID {file_id} is absent from the staged external table")]
    BinaryExternalFileIdMissing { file_id: i32 },
    #[error(
        "logical expected reference does not match staged file ID {actual_file_id} and path ID {actual_path_id}"
    )]
    ExpectedReferenceMismatch {
        actual_file_id: i32,
        actual_path_id: i64,
    },
    #[error("reference destination format does not match its owner")]
    DestinationFormatMismatch,
    #[error("binary PPtr with {fields} fields requires an existing value template")]
    BinaryPPtrTemplateRequired { fields: usize },
    #[error("binary PPtr {role} value {value} does not fit {primitive:?}")]
    BinaryIntegerOutOfRange {
        role: &'static str,
        value: i64,
        primitive: PrimitiveKind,
    },
    #[error("YAML reference mapping is malformed")]
    InvalidYamlReferenceShape,
    #[error("failed to allocate {requested} elements for {resource}")]
    AllocationFailed {
        resource: &'static str,
        requested: usize,
    },
    #[error("arithmetic overflow while accounting for {resource}")]
    ArithmeticOverflow { resource: &'static str },
}

impl From<ReferenceEncodingError> for ReferenceMutationCodecError {
    fn from(source: ReferenceEncodingError) -> Self {
        Self::Encoding(Box::new(source))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;

    use unity_asset_binary::asset::{SerializedFileParser, SerializedType};
    use unity_asset_binary::typetree::{TypeTree, TypeTreeNode};
    use unity_asset_core::{
        AssetLoadLimits, ObjectAddress, ObjectId, SourceId, SourceLocator, WorkspaceId,
    };
    use unity_asset_write::object::SerializedObjectEncoder;
    use unity_asset_write::{BinaryWriter, ByteOrder};

    use super::*;
    use crate::workspace::{
        AssetWorkspace, MutationField, WorkspaceLookup, WorkspaceOptions, WorkspaceSnapshot,
    };

    const V22_BINARY: &[u8] = include_bytes!(
        "../../../../unity-asset-write/tests/fixtures/serialized_file_wire/v22.assets.bin"
    );
    const TRANSFORM_BINARY: &[u8] = include_bytes!(
        "../../../../unity-asset-write/tests/fixtures/serialized_file_wire/transform_hierarchy_v22.assets.bin"
    );

    const MANAGED_OWNER_PATH_ID: i64 = 1;
    const MANAGED_TARGET_PATH_ID: i64 = 2;
    const MANAGED_NAMESPACE: &str = "Tests";
    const MANAGED_ASSEMBLY: &str = "Assembly";

    #[test]
    fn managed_sequence_insert_without_current_uses_the_new_runtime_schema() {
        assert_managed_sequence_reference_lowers(false);
    }

    #[test]
    fn managed_sequence_replace_from_a_to_b_uses_the_new_runtime_schema() {
        assert_managed_sequence_reference_lowers(true);
    }

    #[test]
    fn binary_null_does_not_require_a_valid_external_file_id() {
        let file = SerializedFileParser::from_bytes(V22_BINARY.to_vec()).unwrap();
        let externals = ExternalTableAllocator::new(&file).unwrap();

        assert!(
            binary_external_identifier(
                BinaryPPtr {
                    file_id: i32::MAX,
                    path_id: 0,
                },
                &externals,
            )
            .unwrap()
            .is_none()
        );
        assert!(matches!(
            binary_external_identifier(
                BinaryPPtr {
                    file_id: i32::MAX,
                    path_id: 1,
                },
                &externals,
            ),
            Err(ReferenceMutationCodecError::BinaryExternalFileIdMissing { file_id: i32::MAX })
        ));
    }

    #[test]
    fn binary_reference_replace_compares_the_current_staged_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transforms.assets");
        fs::write(&path, TRANSFORM_BINARY).unwrap();
        let (snapshot, source) = snapshot_with_path(100, &path);
        let file = SerializedFileParser::from_bytes(TRANSFORM_BINARY.to_vec()).unwrap();
        let field_path = FieldPath::root().push_field("m_Father").unwrap();
        let mut selected = None;
        for object in file.objects() {
            let Ok(candidate) = SerializedObjectEncoder::new(&file, object.path_id())
                .and_then(|encoder| encoder.begin_semantic(&mut AssetLoadBudget::default()))
            else {
                continue;
            };
            let Ok(current) = candidate.value_at_path(&field_path) else {
                continue;
            };
            let Ok(schema) = candidate.value_schema_at_path(&field_path) else {
                continue;
            };
            let Some(layout) = schema.pptr_layout() else {
                continue;
            };
            let Ok(raw) = BinaryPPtr::read(current, layout) else {
                continue;
            };
            if raw.file_id == 0 && raw.path_id != 0 {
                selected = Some((candidate, raw.path_id));
                break;
            }
        }
        let (mut candidate, expected_path_id) = selected.expect("fixture child Transform PPtr");
        let owner = RevisionedObjectHandle::new(
            snapshot.workspace_id(),
            snapshot.revision(),
            ObjectId::binary(source, candidate.path_id()).unwrap(),
        )
        .unwrap();
        let expected = ReferenceTarget::object(
            ObjectAddress::binary_at(locator(&snapshot, source), expected_path_id).unwrap(),
        );
        let schema_digest = field_schema_digest(candidate.schema_digest(), &field_path).unwrap();
        let mut externals = ExternalTableAllocator::new(&file).unwrap();
        let mut budget = AssetLoadBudget::default();
        let codec = StagedReferenceMutationCodec::build(&snapshot, &mut budget).unwrap();

        let children_path = FieldPath::root().push_field("m_Children").unwrap();
        let children_schema = candidate.value_schema_at_path(&children_path).unwrap();
        let children_current = candidate.value_at_path(&children_path).unwrap();
        let nested =
            MutationValue::array(vec![MutationValue::reference(expected.clone())]).unwrap();
        let UnityValue::Array(lowered) = codec
            .lower_binary_mutation_value(
                &owner,
                children_schema,
                Some(children_current),
                nested,
                &mut externals,
                &mut budget,
            )
            .unwrap()
        else {
            panic!("binary children replacement must remain an array")
        };
        let child_layout = children_schema.element(0).unwrap().pptr_layout().unwrap();
        assert_eq!(
            BinaryPPtr::read(&lowered[0], child_layout).unwrap().path_id,
            expected_path_id
        );

        codec
            .apply_binary_reference_replace(
                &owner,
                &mut candidate,
                &mut externals,
                0,
                field_path.clone(),
                schema_digest,
                &expected,
                &ReferenceTarget::Null,
                &mut budget,
            )
            .unwrap();
        let current = candidate.value_at_path(&field_path).unwrap();
        let layout = candidate
            .value_schema_at_path(&field_path)
            .unwrap()
            .pptr_layout()
            .unwrap();
        assert_eq!(BinaryPPtr::read(current, layout).unwrap().path_id, 0);

        assert!(matches!(
            codec.apply_binary_reference_replace(
                &owner,
                &mut candidate,
                &mut externals,
                1,
                field_path.clone(),
                schema_digest,
                &expected,
                &ReferenceTarget::Null,
                &mut budget,
            ),
            Err(ReferenceMutationCodecError::ExpectedReferenceMismatch { .. })
        ));
        let current = candidate.value_at_path(&field_path).unwrap();
        let layout = candidate
            .value_schema_at_path(&field_path)
            .unwrap()
            .pptr_layout()
            .unwrap();
        assert_eq!(BinaryPPtr::read(current, layout).unwrap().path_id, 0);
    }

    #[test]
    fn yaml_reference_replace_compares_the_current_staged_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owner.prefab");
        write_yaml(
            &path,
            "--- !u!1 &1\nGameObject:\n  m_Target: {fileID: 2}\n\
             --- !u!1 &2\nGameObject:\n  m_Name: First\n\
             --- !u!1 &3\nGameObject:\n  m_Name: Second\n",
        );
        let (snapshot, source) = snapshot_with_path(101, &path);
        let locator = locator(&snapshot, source);
        let owner = resolved_object(
            &snapshot,
            &ObjectAddress::yaml(locator.clone(), "1").unwrap(),
        );
        let first = ReferenceTarget::object(ObjectAddress::yaml(locator.clone(), "2").unwrap());
        let second = ReferenceTarget::object(ObjectAddress::yaml(locator, "3").unwrap());
        let base = snapshot
            .read_object(&owner, &mut AssetLoadBudget::default())
            .unwrap();
        let mut candidate =
            YamlObjectCandidate::from_workspace_object(base, &mut AssetLoadBudget::default())
                .unwrap();
        let field_path = FieldPath::root().push_field("m_Target").unwrap();
        let current = candidate.class().value_at_path(&field_path).unwrap();
        let schema_digest = yaml_field_schema_digest(
            candidate.class(),
            &field_path,
            current,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let mut budget = AssetLoadBudget::default();
        let codec = StagedReferenceMutationCodec::build(&snapshot, &mut budget).unwrap();

        codec
            .apply_yaml_reference_replace(
                &owner,
                &mut candidate,
                0,
                &field_path,
                schema_digest,
                &first,
                &second,
                &mut budget,
            )
            .unwrap();
        assert_eq!(yaml_file_id(candidate.class(), &field_path), 3);

        let before = candidate
            .class()
            .value_at_path(&field_path)
            .unwrap()
            .clone();
        assert!(matches!(
            codec.apply_yaml_reference_replace(
                &owner,
                &mut candidate,
                1,
                &field_path,
                schema_digest,
                &first,
                &ReferenceTarget::Null,
                &mut budget,
            ),
            Err(ReferenceMutationCodecError::ExpectedReferenceMismatch { .. })
        ));
        assert_eq!(
            candidate.class().value_at_path(&field_path).unwrap(),
            &before
        );

        codec
            .apply_yaml_reference_replace(
                &owner,
                &mut candidate,
                1,
                &field_path,
                schema_digest,
                &second,
                &ReferenceTarget::Null,
                &mut budget,
            )
            .unwrap();
        let UnityValue::Object(null) = candidate.class().value_at_path(&field_path).unwrap() else {
            panic!("reference replacement must remain a mapping")
        };
        assert_eq!(null.len(), 1);
        assert_eq!(null.get(YAML_FILE_ID).and_then(UnityValue::as_i64), Some(0));
    }

    #[test]
    fn nested_yaml_mutation_references_are_lowered_recursively() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested.prefab");
        write_yaml(
            &path,
            "--- !u!1 &1\nGameObject:\n  m_Name: Owner\n\
             --- !u!1 &2\nGameObject:\n  m_Name: Target\n",
        );
        let (snapshot, source) = snapshot_with_path(102, &path);
        let locator = locator(&snapshot, source);
        let owner = resolved_object(
            &snapshot,
            &ObjectAddress::yaml(locator.clone(), "1").unwrap(),
        );
        let target = ReferenceTarget::object(ObjectAddress::yaml(locator, "2").unwrap());
        let nested = MutationValue::object(vec![
            MutationField::new(
                "outer",
                MutationValue::array(vec![
                    MutationValue::object(vec![
                        MutationField::new("target", MutationValue::reference(target)).unwrap(),
                    ])
                    .unwrap(),
                ])
                .unwrap(),
            )
            .unwrap(),
        ])
        .unwrap();
        let mut budget = AssetLoadBudget::default();
        let codec = StagedReferenceMutationCodec::build(&snapshot, &mut budget).unwrap();

        let lowered = codec
            .lower_yaml_mutation_value(&owner, None, nested, &mut budget)
            .unwrap();

        let UnityValue::Object(root) = lowered else {
            panic!("root must remain an object")
        };
        let UnityValue::Array(outer) = root.get("outer").unwrap() else {
            panic!("outer must remain an array")
        };
        let UnityValue::Object(element) = &outer[0] else {
            panic!("array element must remain an object")
        };
        let UnityValue::Object(reference) = element.get("target").unwrap() else {
            panic!("logical reference must lower to a YAML mapping")
        };
        assert_eq!(
            reference.get(YAML_FILE_ID).and_then(UnityValue::as_i64),
            Some(2)
        );
        assert_eq!(reference.len(), 1, "local references omit GUID and type");
    }

    #[test]
    fn yaml_destination_enforces_guid_type_pair_and_preserves_alias_family() {
        let template = UnityValue::Object(IndexMap::from([(
            YAML_MEMBER_FILE_ID.to_owned(),
            UnityValue::Integer(0),
        )]));
        let template = YamlPPtr::read(&template).unwrap();
        let mut budget = AssetLoadBudget::default();
        let UnityValue::Object(external) = yaml_pptr_value(
            ReferenceDestination::YamlExternal {
                file_id: 7,
                guid: [0xab; 16],
                type_id: 17,
            },
            Some(template),
            &mut budget,
        )
        .unwrap() else {
            panic!("external YAML reference must be a mapping")
        };

        assert_eq!(
            external
                .get(YAML_MEMBER_FILE_ID)
                .and_then(UnityValue::as_i64),
            Some(7)
        );
        assert_eq!(
            external.get(YAML_MEMBER_TYPE).and_then(UnityValue::as_i64),
            Some(17)
        );
        assert_eq!(
            external.get(YAML_MEMBER_GUID).and_then(UnityValue::as_str),
            Some("abababababababababababababababab")
        );
        assert_eq!(external.len(), 3);

        let external = UnityValue::Object(external);
        let parsed = YamlPPtr::read(&external).unwrap();
        let UnityValue::Object(local) = yaml_pptr_value(
            ReferenceDestination::YamlLocal { file_id: 9 },
            Some(parsed),
            &mut budget,
        )
        .unwrap() else {
            panic!("local YAML reference must be a mapping")
        };
        assert_eq!(local.len(), 1, "local references must drop GUID and type");
    }

    #[test]
    fn lowering_allocations_charge_actual_container_capacity() {
        let mut measured = AssetLoadBudget::default();
        let array = allocate_array(3, &mut measured).unwrap();
        let expected_array = vec_allocation_bytes::<UnityValue>(array.capacity()).unwrap();
        assert_eq!(measured.usage().bytes, expected_array);
        assert_eq!(measured.usage().members, 3);

        let mut measured = AssetLoadBudget::default();
        let object = allocate_object(3, &mut measured).unwrap();
        let expected_object =
            index_map_allocation_bytes::<String, UnityValue>(object.capacity()).unwrap();
        assert_eq!(measured.usage().bytes, expected_object);
        assert_eq!(measured.usage().members, 3);

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: expected_object - 1,
            max_members: 3,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            allocate_object(3, &mut short),
            Err(ReferenceMutationCodecError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(short.usage().bytes, 0);
        assert_eq!(short.usage().members, 0);
    }

    fn assert_managed_sequence_reference_lowers(replace_existing: bool) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("managed.assets");
        let bytes = managed_sequence_fixture();
        fs::write(&path, &bytes).unwrap();
        let (snapshot, source) = snapshot_with_path(103, &path);
        let source_locator = locator(&snapshot, source);
        let owner = resolved_object(
            &snapshot,
            &ObjectAddress::binary_at(source_locator.clone(), MANAGED_OWNER_PATH_ID).unwrap(),
        );
        let target = ReferenceTarget::object(
            ObjectAddress::binary_at(source_locator, MANAGED_TARGET_PATH_ID).unwrap(),
        );

        let file = SerializedFileParser::from_bytes(bytes).unwrap();
        let candidate = SerializedObjectEncoder::new(&file, MANAGED_OWNER_PATH_ID)
            .unwrap()
            .begin_semantic(&mut AssetLoadBudget::default())
            .unwrap();
        let sequence_path = FieldPath::root().push_field("m_Refs").unwrap();
        let sequence = candidate
            .value_at_path(&sequence_path)
            .unwrap()
            .as_array()
            .unwrap();
        let current = replace_existing.then(|| &sequence[0]);
        let element_schema = candidate
            .value_schema_at_path(&sequence_path)
            .unwrap()
            .element(0)
            .unwrap();
        let replacement = managed_b_mutation(target);
        let mut externals = ExternalTableAllocator::new(&file).unwrap();
        let mut budget = AssetLoadBudget::default();
        let codec = StagedReferenceMutationCodec::build(&snapshot, &mut budget).unwrap();

        let lowered = codec
            .lower_binary_mutation_value(
                &owner,
                element_schema,
                current,
                replacement,
                &mut externals,
                &mut budget,
            )
            .unwrap();

        let UnityValue::Object(fields) = &lowered else {
            panic!("managed replacement must remain an object")
        };
        let UnityValue::Object(type_fields) = fields.get("type").unwrap() else {
            panic!("managed replacement must retain its runtime type")
        };
        assert_eq!(
            type_fields.get("class").and_then(UnityValue::as_str),
            Some("ManagedB")
        );
        let UnityValue::Object(payload) = fields.get("data").unwrap() else {
            panic!("managed replacement must retain its payload")
        };
        let target_schema = element_schema
            .field_for_value("data", &lowered)
            .unwrap()
            .field("target")
            .unwrap();
        let target_layout = target_schema.pptr_layout().unwrap();
        let raw = BinaryPPtr::read(payload.get("target").unwrap(), target_layout).unwrap();
        assert_eq!(raw.file_id, 0);
        assert_eq!(raw.path_id, MANAGED_TARGET_PATH_ID);
    }

    fn managed_b_mutation(target: ReferenceTarget) -> MutationValue {
        MutationValue::object(vec![
            MutationField::new(
                "type",
                MutationValue::object(vec![
                    MutationField::new("class", MutationValue::string("ManagedB").unwrap())
                        .unwrap(),
                    MutationField::new("ns", MutationValue::string(MANAGED_NAMESPACE).unwrap())
                        .unwrap(),
                    MutationField::new("asm", MutationValue::string(MANAGED_ASSEMBLY).unwrap())
                        .unwrap(),
                ])
                .unwrap(),
            )
            .unwrap(),
            MutationField::new(
                "data",
                MutationValue::object(vec![
                    MutationField::new("target", MutationValue::reference(target)).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap(),
        ])
        .unwrap()
    }

    fn managed_sequence_fixture() -> Vec<u8> {
        let referenced = fixture_referenced_object("data");
        let mut array = fixture_record(
            "Array",
            "Array",
            vec![fixture_node("int", "size"), referenced],
        );
        array.type_flags = 1;
        let root = fixture_record(
            "ManagedSequenceFixture",
            "Base",
            vec![fixture_record("vector", "m_Refs", vec![array])],
        );
        let mut root_type = SerializedType::new(28);
        root_type.type_tree = fixture_tree(root);
        root_type.old_type_hash = [0x31; 16];

        let mut managed_a = SerializedType::new(114);
        managed_a.script_type_index = 0;
        managed_a.class_name = "ManagedA".to_owned();
        managed_a.namespace = MANAGED_NAMESPACE.to_owned();
        managed_a.assembly_name = MANAGED_ASSEMBLY.to_owned();
        managed_a.script_id = [0x41; 16];
        managed_a.old_type_hash = [0x51; 16];
        managed_a.type_tree = fixture_tree(fixture_record(
            "ManagedA",
            "ManagedA",
            vec![fixture_node("int", "m_Marker")],
        ));

        let mut pointer = fixture_node("PPtr<Object>", "target");
        pointer.children = vec![
            fixture_node("int", "m_FileID"),
            fixture_node("SInt64", "m_PathID"),
        ];
        let mut managed_b = SerializedType::new(114);
        managed_b.script_type_index = 0;
        managed_b.class_name = "ManagedB".to_owned();
        managed_b.namespace = MANAGED_NAMESPACE.to_owned();
        managed_b.assembly_name = MANAGED_ASSEMBLY.to_owned();
        managed_b.script_id = [0x42; 16];
        managed_b.old_type_hash = [0x52; 16];
        managed_b.type_tree = fixture_tree(fixture_record("ManagedB", "ManagedB", vec![pointer]));

        let mut owner = BinaryWriter::new(ByteOrder::Little);
        owner.write_i32(1);
        owner.write_aligned_string("ManagedA").unwrap();
        owner.write_aligned_string(MANAGED_NAMESPACE).unwrap();
        owner.write_aligned_string(MANAGED_ASSEMBLY).unwrap();
        owner.write_i32(7);
        let owner = owner.into_result().unwrap();

        let mut target = BinaryWriter::new(ByteOrder::Little);
        target.write_i32(0);
        let target = target.into_result().unwrap();
        let target_offset = owner.len().checked_add(15).unwrap() & !15;
        let mut payload = owner.clone();
        payload.resize(target_offset, 0);
        payload.extend_from_slice(&target);

        let mut metadata = BinaryWriter::new(ByteOrder::Little);
        metadata.write_string_to_null("2022.3.0f1");
        metadata.write_i32(13);
        metadata.write_bool(true);
        metadata.write_i32(1);
        write_fixture_serialized_type(&mut metadata, &root_type, false);
        metadata.write_i32(2);
        metadata.align_stream(4);
        write_fixture_object(&mut metadata, MANAGED_OWNER_PATH_ID, 0, owner.len());
        write_fixture_object(
            &mut metadata,
            MANAGED_TARGET_PATH_ID,
            target_offset,
            target.len(),
        );
        metadata.write_i32(0);
        metadata.write_i32(0);
        metadata.write_i32(2);
        write_fixture_serialized_type(&mut metadata, &managed_a, true);
        write_fixture_serialized_type(&mut metadata, &managed_b, true);
        metadata.write_string_to_null("");
        let metadata = metadata.into_result().unwrap();

        let data_offset = 48_usize.checked_add(metadata.len()).unwrap();
        let data_offset = data_offset.checked_add(15).unwrap() & !15;
        let file_size = data_offset.checked_add(payload.len()).unwrap();
        let mut header = BinaryWriter::new(ByteOrder::Big);
        header.write_u32(0);
        header.write_u32(0);
        header.write_u32(22);
        header.write_u32(0);
        header.write_u8(0);
        header.write(&[0; 3]);
        header.write_u32(u32::try_from(metadata.len()).unwrap());
        header.write_i64(i64::try_from(file_size).unwrap());
        header.write_i64(i64::try_from(data_offset).unwrap());
        header.write_i64(0);
        let mut output = header.into_result().unwrap();
        assert_eq!(output.len(), 48);
        output.extend_from_slice(&metadata);
        output.resize(data_offset, 0);
        output.extend_from_slice(&payload);
        output
    }

    fn fixture_node(type_name: &str, name: &str) -> TypeTreeNode {
        let byte_size = match type_name {
            "int" => 4,
            "SInt64" => 8,
            _ => -1,
        };
        let mut node = TypeTreeNode::with_info(type_name.to_owned(), name.to_owned(), byte_size);
        node.version = 1;
        node
    }

    fn fixture_record(type_name: &str, name: &str, children: Vec<TypeTreeNode>) -> TypeTreeNode {
        let mut node = fixture_node(type_name, name);
        node.children = children;
        node
    }

    fn fixture_referenced_object(name: &str) -> TypeTreeNode {
        let type_node = fixture_record(
            "ReferencedObjectType",
            "type",
            vec![
                fixture_node("string", "class"),
                fixture_node("string", "ns"),
                fixture_node("string", "asm"),
            ],
        );
        fixture_record(
            "ReferencedObject",
            name,
            vec![type_node, fixture_node("ReferencedObjectData", "data")],
        )
    }

    fn fixture_tree(root: TypeTreeNode) -> TypeTree {
        let mut tree = TypeTree::new();
        tree.add_node(root);
        tree
    }

    fn write_fixture_object(writer: &mut BinaryWriter, path_id: i64, offset: usize, size: usize) {
        writer.write_i64(path_id);
        writer.write_i64(i64::try_from(offset).unwrap());
        writer.write_u32(u32::try_from(size).unwrap());
        writer.write_i32(0);
    }

    fn write_fixture_serialized_type(
        writer: &mut BinaryWriter,
        serialized_type: &SerializedType,
        reference_type: bool,
    ) {
        writer.write_i32(serialized_type.class_id);
        writer.write_bool(serialized_type.is_stripped_type);
        writer.write_i16(serialized_type.script_type_index);
        if reference_type {
            writer.write(&serialized_type.script_id);
        }
        writer.write(&serialized_type.old_type_hash);
        write_fixture_type_tree(writer, &serialized_type.type_tree);
        if reference_type {
            writer.write_string_to_null(&serialized_type.class_name);
            writer.write_string_to_null(&serialized_type.namespace);
            writer.write_string_to_null(&serialized_type.assembly_name);
        } else {
            writer.write_i32(i32::try_from(serialized_type.type_dependencies.len()).unwrap());
            for dependency in &serialized_type.type_dependencies {
                writer.write_i32(*dependency);
            }
        }
    }

    fn write_fixture_type_tree(writer: &mut BinaryWriter, tree: &TypeTree) {
        let mut flattened = Vec::new();
        flatten_fixture_node(&tree.nodes[0], 0, &mut flattened);
        let mut offsets = HashMap::new();
        let mut strings = Vec::new();
        for (_, node) in &flattened {
            intern_fixture_string(&node.type_name, &mut offsets, &mut strings);
            intern_fixture_string(&node.name, &mut offsets, &mut strings);
        }

        writer.write_i32(i32::try_from(flattened.len()).unwrap());
        writer.write_i32(i32::try_from(strings.len()).unwrap());
        for (index, (level, node)) in flattened.iter().enumerate() {
            writer.write_i16(i16::try_from(node.version).unwrap());
            writer.write_u8(*level);
            writer.write_u8(u8::try_from(node.type_flags).unwrap());
            writer.write_u32(offsets[node.type_name.as_str()]);
            writer.write_u32(offsets[node.name.as_str()]);
            writer.write_i32(node.byte_size);
            writer.write_i32(i32::try_from(index).unwrap());
            writer.write_i32(node.meta_flags);
            writer.write_u64(node.ref_type_hash);
        }
        writer.write(&strings);
    }

    fn flatten_fixture_node<'node>(
        node: &'node TypeTreeNode,
        level: u8,
        flattened: &mut Vec<(u8, &'node TypeTreeNode)>,
    ) {
        flattened.push((level, node));
        let child_level = level.checked_add(1).unwrap();
        for child in &node.children {
            flatten_fixture_node(child, child_level, flattened);
        }
    }

    fn intern_fixture_string(
        value: &str,
        offsets: &mut HashMap<String, u32>,
        strings: &mut Vec<u8>,
    ) {
        if offsets.contains_key(value) {
            return;
        }
        let offset = u32::try_from(strings.len()).unwrap();
        strings.extend_from_slice(value.as_bytes());
        strings.push(0);
        offsets.insert(value.to_owned(), offset);
    }

    fn snapshot_with_path(id: u128, path: &Path) -> (WorkspaceSnapshot, SourceId) {
        let mut workspace = AssetWorkspace::with_workspace_id(
            WorkspaceId::from_u128(id).unwrap(),
            WorkspaceOptions::default(),
        )
        .unwrap();
        let source = workspace
            .load_path(path, &mut AssetLoadBudget::default())
            .unwrap();
        (workspace.snapshot(), source)
    }

    fn locator(snapshot: &WorkspaceSnapshot, source: SourceId) -> SourceLocator {
        match snapshot
            .source(source, &mut AssetLoadBudget::default())
            .unwrap()
        {
            WorkspaceLookup::Resolved(source) => source.locator().clone(),
            other => panic!("source lookup did not resolve: {other:?}"),
        }
    }

    fn resolved_object(
        snapshot: &WorkspaceSnapshot,
        address: &ObjectAddress,
    ) -> RevisionedObjectHandle {
        match snapshot
            .resolve_object(address, &mut AssetLoadBudget::default())
            .unwrap()
        {
            WorkspaceLookup::Resolved(object) => object,
            other => panic!("object lookup did not resolve: {other:?}"),
        }
    }

    fn yaml_file_id(class: &unity_asset_core::UnityClass, path: &FieldPath) -> i64 {
        let UnityValue::Object(reference) = class.value_at_path(path).unwrap() else {
            panic!("reference must be a mapping")
        };
        reference
            .get(YAML_FILE_ID)
            .and_then(UnityValue::as_i64)
            .unwrap()
    }

    fn write_yaml(path: &Path, documents: &str) {
        fs::write(
            path,
            format!("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n{documents}"),
        )
        .unwrap();
    }
}
