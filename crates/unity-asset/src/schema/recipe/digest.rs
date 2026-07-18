use unity_asset_core::{
    AssetLoadBudget, DigestV1, DigestV1Builder, FieldPath, FieldPathSegment, UnityClass, UnityValue,
};

use crate::workspace::MutationPlanError;

use super::contract::RecipeError;

const FIELD_SCHEMA_DOMAIN: &[u8] = b"unity-asset:field-schema:v1\0";
const VALUE_DOMAIN: &[u8] = b"unity-asset:semantic-value:v1\0";
const YAML_SCHEMA_DOMAIN: &[u8] = b"unity-asset:yaml-schema:v1\0";
const YAML_FIELD_SCHEMA_DOMAIN: &[u8] = b"unity-asset:yaml-field-schema:v1\0";
const MAX_RECIPE_VALUE_DEPTH: u32 = 59;

pub(crate) fn digest_yaml_schema(
    class: &UnityClass,
    budget: &mut AssetLoadBudget,
) -> Result<DigestV1, RecipeError> {
    let body_len = schema_class_len(class, budget)?;
    let total = checked_add_len(YAML_SCHEMA_DOMAIN.len() as u64, body_len)?;
    let mut builder = DigestV1Builder::new(total);
    builder.update(YAML_SCHEMA_DOMAIN)?;
    write_i32(&mut builder, class.class_id)?;
    write_bytes(&mut builder, class.class_name.as_bytes())?;
    write_schema_object(&mut builder, class.properties(), budget, 1)?;
    Ok(builder.finalize()?)
}

pub(super) fn yaml_field_schema_digest(
    class: &UnityClass,
    path: &FieldPath,
    value: &UnityValue,
    budget: &mut AssetLoadBudget,
) -> Result<DigestV1, RecipeError> {
    let body_len = checked_add_len(
        checked_add_len(4, framed_len(class.class_name.as_bytes())?)?,
        checked_add_len(
            field_path_encoded_len(path)?,
            schema_value_len(value, 1, budget)?,
        )?,
    )?;
    let total = checked_add_len(YAML_FIELD_SCHEMA_DOMAIN.len() as u64, body_len)?;
    let mut builder = DigestV1Builder::new(total);
    builder.update(YAML_FIELD_SCHEMA_DOMAIN)?;
    write_i32(&mut builder, class.class_id)?;
    write_bytes(&mut builder, class.class_name.as_bytes())?;
    write_field_path(&mut builder, path)?;
    write_schema_value(&mut builder, value, budget, 1)?;
    Ok(builder.finalize()?)
}

pub(super) fn field_schema_digest(
    schema_digest: DigestV1,
    path: &FieldPath,
) -> Result<DigestV1, RecipeError> {
    let path_len = field_path_encoded_len(path)?;
    let total = checked_add_len(
        checked_add_len(FIELD_SCHEMA_DOMAIN.len() as u64, DigestV1::BYTE_LEN as u64)?,
        path_len,
    )?;
    let mut builder = DigestV1Builder::new(total);
    builder.update(FIELD_SCHEMA_DOMAIN)?;
    builder.update(schema_digest.as_bytes())?;
    write_field_path(&mut builder, path)?;
    Ok(builder.finalize()?)
}

pub(super) fn semantic_value_digest(
    value: &UnityValue,
    budget: &mut AssetLoadBudget,
) -> Result<DigestV1, RecipeError> {
    let body_len = semantic_value_len(value, 1, budget)?;
    let total = checked_add_len(VALUE_DOMAIN.len() as u64, body_len)?;
    let mut builder = DigestV1Builder::new(total);
    builder.update(VALUE_DOMAIN)?;
    write_semantic_value(&mut builder, value, budget, 1)?;
    Ok(builder.finalize()?)
}

fn schema_class_len(class: &UnityClass, budget: &mut AssetLoadBudget) -> Result<u64, RecipeError> {
    let mut total = 4_u64;
    total = checked_add_len(total, framed_len(class.class_name.as_bytes())?)?;
    checked_add_len(total, schema_object_len(class.properties(), 1, budget)?)
}

fn schema_value_len(
    value: &UnityValue,
    depth: u32,
    budget: &mut AssetLoadBudget,
) -> Result<u64, RecipeError> {
    observe_value(depth, budget)?;
    match value {
        UnityValue::Null
        | UnityValue::Bool(_)
        | UnityValue::Integer(_)
        | UnityValue::Unsigned(_)
        | UnityValue::Float(_)
        | UnityValue::String(_)
        | UnityValue::Bytes(_) => Ok(1),
        UnityValue::Array(values) => {
            let mut total = 2_u64;
            if let Some(value) = values.first() {
                total = checked_add_len(total, schema_value_len(value, depth + 1, budget)?)?;
            }
            Ok(total)
        }
        UnityValue::Object(fields) => schema_object_body_len(fields, depth, budget),
    }
}

fn schema_object_len(
    fields: &indexmap::IndexMap<String, UnityValue>,
    depth: u32,
    budget: &mut AssetLoadBudget,
) -> Result<u64, RecipeError> {
    observe_value(depth, budget)?;
    schema_object_body_len(fields, depth, budget)
}

fn schema_object_body_len(
    fields: &indexmap::IndexMap<String, UnityValue>,
    depth: u32,
    budget: &mut AssetLoadBudget,
) -> Result<u64, RecipeError> {
    let mut total = 1_u64 + 8;
    for (name, value) in fields {
        total = checked_add_len(total, framed_len(name.as_bytes())?)?;
        total = checked_add_len(total, schema_value_len(value, depth + 1, budget)?)?;
    }
    Ok(total)
}

fn semantic_value_len(
    value: &UnityValue,
    depth: u32,
    budget: &mut AssetLoadBudget,
) -> Result<u64, RecipeError> {
    observe_value(depth, budget)?;
    match value {
        UnityValue::Null => Ok(1),
        UnityValue::Bool(_) => Ok(2),
        UnityValue::Integer(_) | UnityValue::Unsigned(_) | UnityValue::Float(_) => Ok(9),
        UnityValue::String(value) => checked_add_len(1, framed_len(value.as_bytes())?),
        UnityValue::Bytes(value) => checked_add_len(1, framed_len(value)?),
        UnityValue::Array(values) => {
            let mut total = 1_u64 + 8;
            for value in values {
                total = checked_add_len(total, semantic_value_len(value, depth + 1, budget)?)?;
            }
            Ok(total)
        }
        UnityValue::Object(fields) => {
            let mut total = 1_u64 + 8;
            for (name, value) in fields {
                total = checked_add_len(total, framed_len(name.as_bytes())?)?;
                total = checked_add_len(total, semantic_value_len(value, depth + 1, budget)?)?;
            }
            Ok(total)
        }
    }
}

fn write_schema_object(
    builder: &mut DigestV1Builder,
    fields: &indexmap::IndexMap<String, UnityValue>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<(), RecipeError> {
    check_depth(depth)?;
    builder.update(&[8])?;
    write_u64(builder, fields.len() as u64)?;
    let sorted = sorted_fields(fields, budget)?;
    for (name, value) in sorted {
        write_bytes(builder, name.as_bytes())?;
        write_schema_value(builder, value, budget, depth + 1)?;
    }
    Ok(())
}

fn write_schema_value(
    builder: &mut DigestV1Builder,
    value: &UnityValue,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<(), RecipeError> {
    check_depth(depth)?;
    match value {
        UnityValue::Null => builder.update(&[0])?,
        UnityValue::Bool(_) => builder.update(&[1])?,
        UnityValue::Integer(_) => builder.update(&[2])?,
        UnityValue::Unsigned(_) => builder.update(&[3])?,
        UnityValue::Float(_) => builder.update(&[4])?,
        UnityValue::String(_) => builder.update(&[5])?,
        UnityValue::Bytes(_) => builder.update(&[6])?,
        UnityValue::Array(values) => {
            builder.update(&[7])?;
            builder.update(&[u8::from(!values.is_empty())])?;
            if let Some(value) = values.first() {
                write_schema_value(builder, value, budget, depth + 1)?;
            }
        }
        UnityValue::Object(fields) => {
            builder.update(&[8])?;
            write_u64(builder, fields.len() as u64)?;
            let sorted = sorted_fields(fields, budget)?;
            for (name, value) in sorted {
                write_bytes(builder, name.as_bytes())?;
                write_schema_value(builder, value, budget, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn write_semantic_value(
    builder: &mut DigestV1Builder,
    value: &UnityValue,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<(), RecipeError> {
    check_depth(depth)?;
    match value {
        UnityValue::Null => builder.update(&[0])?,
        UnityValue::Bool(value) => builder.update(&[1, u8::from(*value)])?,
        UnityValue::Integer(value) => {
            builder.update(&[2])?;
            builder.update(&value.to_le_bytes())?;
        }
        UnityValue::Unsigned(value) => {
            builder.update(&[3])?;
            builder.update(&value.to_le_bytes())?;
        }
        UnityValue::Float(value) => {
            builder.update(&[4])?;
            builder.update(&value.to_bits().to_le_bytes())?;
        }
        UnityValue::String(value) => {
            builder.update(&[5])?;
            write_bytes(builder, value.as_bytes())?;
        }
        UnityValue::Bytes(value) => {
            builder.update(&[6])?;
            write_bytes(builder, value)?;
        }
        UnityValue::Array(values) => {
            builder.update(&[7])?;
            write_u64(builder, values.len() as u64)?;
            for value in values {
                write_semantic_value(builder, value, budget, depth + 1)?;
            }
        }
        UnityValue::Object(fields) => {
            builder.update(&[8])?;
            write_u64(builder, fields.len() as u64)?;
            let sorted = sorted_fields(fields, budget)?;
            for (name, value) in sorted {
                write_bytes(builder, name.as_bytes())?;
                write_semantic_value(builder, value, budget, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn sorted_fields<'value>(
    fields: &'value indexmap::IndexMap<String, UnityValue>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<(&'value str, &'value UnityValue)>, RecipeError> {
    let bytes = fields
        .len()
        .checked_mul(size_of::<(&str, &UnityValue)>())
        .ok_or(RecipeError::DigestLengthOverflow)?;
    budget.check_bytes(u64::try_from(bytes).map_err(|_| RecipeError::DigestLengthOverflow)?)?;
    let mut sorted = Vec::new();
    sorted
        .try_reserve_exact(fields.len())
        .map_err(|error| RecipeError::AllocationFailed {
            resource: "recipe digest field sort",
            requested: fields.len(),
            message: error.to_string(),
        })?;
    sorted.extend(fields.iter().map(|(name, value)| (name.as_str(), value)));
    sorted.sort_unstable_by(|left, right| left.0.cmp(right.0));
    budget.consume_bytes(u64::try_from(bytes).map_err(|_| RecipeError::DigestLengthOverflow)?)?;
    Ok(sorted)
}

pub(super) fn observe_value(depth: u32, budget: &mut AssetLoadBudget) -> Result<(), RecipeError> {
    check_depth(depth)?;
    budget.observe_depth(depth)?;
    budget.consume_entries(1)?;
    Ok(())
}

fn check_depth(depth: u32) -> Result<(), RecipeError> {
    if depth > MAX_RECIPE_VALUE_DEPTH {
        return Err(MutationPlanError::ValueDepthExceeded {
            maximum: MAX_RECIPE_VALUE_DEPTH,
            actual: depth,
        }
        .into());
    }
    Ok(())
}

fn field_path_encoded_len(path: &FieldPath) -> Result<u64, RecipeError> {
    let mut total = 8_u64;
    for segment in path.segments() {
        total = checked_add_len(total, 1)?;
        total = match segment {
            FieldPathSegment::Field(name) => checked_add_len(total, framed_len(name.as_bytes())?)?,
            FieldPathSegment::Index(_) => checked_add_len(total, 4)?,
        };
    }
    Ok(total)
}

fn write_field_path(builder: &mut DigestV1Builder, path: &FieldPath) -> Result<(), RecipeError> {
    write_u64(builder, path.segments().len() as u64)?;
    for segment in path.segments() {
        match segment {
            FieldPathSegment::Field(name) => {
                builder.update(&[0])?;
                write_bytes(builder, name.as_bytes())?;
            }
            FieldPathSegment::Index(index) => {
                builder.update(&[1])?;
                builder.update(&index.to_le_bytes())?;
            }
        }
    }
    Ok(())
}

fn framed_len(bytes: &[u8]) -> Result<u64, RecipeError> {
    DigestV1Builder::framed_len(bytes).map_err(|_| RecipeError::DigestLengthOverflow)
}

fn checked_add_len(left: u64, right: u64) -> Result<u64, RecipeError> {
    left.checked_add(right)
        .ok_or(RecipeError::DigestLengthOverflow)
}

fn write_bytes(builder: &mut DigestV1Builder, bytes: &[u8]) -> Result<(), RecipeError> {
    builder.update_framed(bytes)?;
    Ok(())
}

fn write_u64(builder: &mut DigestV1Builder, value: u64) -> Result<(), RecipeError> {
    builder.update(&value.to_le_bytes())?;
    Ok(())
}

fn write_i32(builder: &mut DigestV1Builder, value: i32) -> Result<(), RecipeError> {
    builder.update(&value.to_le_bytes())?;
    Ok(())
}
