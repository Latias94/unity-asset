use unity_asset_core::{AssetLoadBudget, FieldPath, UnityValue, class_ids, class_names};

use crate::workspace::{GenericMutation, MutationField, MutationValue};

use super::recipe::{
    RecipeError, RecipeId, RecipeLowering, RecipeObject, RecipeOutputBuilder, SchemaRecipePlanner,
    SchemaVariantId, ensure_finite, validate_recipe_provenance, value_kind,
};

mod reparent;

pub(crate) use reparent::validate_hierarchy_target;
pub use reparent::{
    HierarchyDestinationV1, HierarchyIntentV1, HierarchyPlacementV1, HierarchyRecipe,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vector2 {
    x: f64,
    y: f64,
}

impl Vector2 {
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    #[must_use]
    pub const fn x(self) -> f64 {
        self.x
    }

    #[must_use]
    pub const fn y(self) -> f64 {
        self.y
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vector3 {
    x: f64,
    y: f64,
    z: f64,
}

impl Vector3 {
    #[must_use]
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quaternion {
    x: f64,
    y: f64,
    z: f64,
    w: f64,
}

impl Quaternion {
    #[must_use]
    pub const fn new(x: f64, y: f64, z: f64, w: f64) -> Self {
        Self { x, y, z, w }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransformChange {
    LocalPosition(Vector3),
    LocalRotation(Quaternion),
    LocalScale(Vector3),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RectTransformChange {
    AnchoredPosition(Vector2),
    SizeDelta(Vector2),
    AnchorMin(Vector2),
    AnchorMax(Vector2),
    Pivot(Vector2),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TransformRecipe;

impl TransformRecipe {
    pub fn lower_transform(
        planner: &SchemaRecipePlanner<'_>,
        transform: &RecipeObject,
        change: TransformChange,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecipeLowering, RecipeError> {
        planner.validate_object(transform)?;
        let mut output = RecipeOutputBuilder::new(budget);
        validate_transform_or_rect_transform_class(transform, &mut output)?;
        let (field, replacement) = match change {
            TransformChange::LocalPosition(value) => {
                ensure_finite(&[value.x, value.y, value.z])?;
                ("m_LocalPosition", vector3_value(value, &mut output)?)
            }
            TransformChange::LocalRotation(value) => {
                ensure_finite(&[value.x, value.y, value.z, value.w])?;
                ("m_LocalRotation", quaternion_value(value, &mut output)?)
            }
            TransformChange::LocalScale(value) => {
                ensure_finite(&[value.x, value.y, value.z])?;
                ("m_LocalScale", vector3_value(value, &mut output)?)
            }
        };
        lower_field(
            planner,
            transform,
            field,
            replacement,
            SchemaVariantId::Transform,
            &mut output,
        )
    }

    pub fn lower_rect_transform(
        planner: &SchemaRecipePlanner<'_>,
        transform: &RecipeObject,
        change: RectTransformChange,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecipeLowering, RecipeError> {
        planner.validate_object(transform)?;
        let mut output = RecipeOutputBuilder::new(budget);
        validate_rect_transform_class(transform, &mut output)?;
        let variant = rect_transform_variant(transform, &mut output)?;
        let (field, value, variant) = match change {
            RectTransformChange::AnchoredPosition(value) => (
                match variant {
                    SchemaVariantId::RectTransformAnchoredPosition => "m_AnchoredPosition",
                    SchemaVariantId::RectTransformLegacyPosition => "m_Position",
                    _ => {
                        return Err(RecipeError::UnsupportedSchema {
                            variant: "RectTransform position field",
                        });
                    }
                },
                value,
                variant,
            ),
            RectTransformChange::SizeDelta(value) => ("m_SizeDelta", value, variant),
            RectTransformChange::AnchorMin(value) => ("m_AnchorMin", value, variant),
            RectTransformChange::AnchorMax(value) => ("m_AnchorMax", value, variant),
            RectTransformChange::Pivot(value) => ("m_Pivot", value, variant),
        };
        ensure_finite(&[value.x, value.y])?;
        lower_field(
            planner,
            transform,
            field,
            vector2_value(value, &mut output)?,
            variant,
            &mut output,
        )
    }
}

fn rect_transform_variant(
    object: &RecipeObject,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<SchemaVariantId, RecipeError> {
    let modern = output.field_path(&["m_AnchoredPosition"])?;
    let legacy = output.field_path(&["m_Position"])?;
    match (object.field(&modern), object.field(&legacy)) {
        (Some(_), None) => Ok(SchemaVariantId::RectTransformAnchoredPosition),
        (None, Some(_)) => Ok(SchemaVariantId::RectTransformLegacyPosition),
        (Some(_), Some(_)) => Err(RecipeError::AmbiguousFieldVariant {
            first: "m_AnchoredPosition",
            second: "m_Position",
        }),
        (None, None) => Err(RecipeError::MissingField { path: modern }),
    }
}

fn lower_field(
    planner: &SchemaRecipePlanner<'_>,
    transform: &RecipeObject,
    field: &'static str,
    replacement: MutationValue,
    variant: SchemaVariantId,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<RecipeLowering, RecipeError> {
    let path = output.field_path(&[field])?;
    validate_vector_shape(
        transform.require_field(&path, output)?,
        &path,
        replacement.view(),
        output,
    )?;
    let guard = transform.field_guard(&path, output.budget())?;
    let action = GenericMutation::FieldReplace {
        target: output.address(transform.address())?,
        path,
        guard,
        replacement,
    };
    let mut actions = output.vec::<GenericMutation>(1, "Transform recipe actions")?;
    actions.push(action);
    RecipeLowering::changed(
        RecipeId::TransformV1,
        variant,
        transform.fragment(planner, Vec::new(), actions, output)?,
    )
}

fn validate_vector_shape(
    existing: &UnityValue,
    path: &FieldPath,
    replacement: crate::workspace::MutationValueRef<'_>,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    let Some(fields) = existing.as_object() else {
        return Err(RecipeError::WrongFieldShape {
            path: output.path(path)?,
            expected: "a vector or quaternion object",
            actual: value_kind(existing),
        });
    };
    let crate::workspace::MutationValueRef::Object(expected) = replacement else {
        return Err(RecipeError::InvalidPayload {
            reason: "transform replacement must be an object",
        });
    };
    if expected.iter().all(|field| {
        fields
            .get(field.name())
            .and_then(UnityValue::as_f64)
            .is_some()
    }) {
        Ok(())
    } else {
        Err(RecipeError::WrongFieldShape {
            path: output.path(path)?,
            expected: "numeric components matching the requested transform value",
            actual: value_kind(existing),
        })
    }
}

fn validate_transform_or_rect_transform_class(
    object: &RecipeObject,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    validate_recipe_provenance(object)?;
    let class = object.class();
    if is_transform_or_rect_transform_class(class) {
        Ok(())
    } else {
        Err(RecipeError::WrongClass {
            expected_id: class_ids::TRANSFORM,
            expected_name: "Transform or RectTransform",
            actual_id: class.class_id(),
            actual_name: output.string(class.class_name(), "recipe class diagnostic")?,
        })
    }
}

fn is_transform_or_rect_transform_class(class: &unity_asset_core::UnityClass) -> bool {
    match class.class_id() {
        class_ids::TRANSFORM => class.class_name() == class_names::TRANSFORM,
        class_ids::RECT_TRANSFORM => class.class_name() == class_names::RECT_TRANSFORM,
        _ => false,
    }
}

fn is_transform_or_rect_transform_class_id(class_id: i32) -> bool {
    matches!(class_id, class_ids::TRANSFORM | class_ids::RECT_TRANSFORM)
}

fn validate_rect_transform_class(
    object: &RecipeObject,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    let class = object.class();
    if class.class_id() == class_ids::RECT_TRANSFORM
        && class.class_name() == class_names::RECT_TRANSFORM
    {
        validate_recipe_provenance(object)
    } else {
        Err(RecipeError::WrongClass {
            expected_id: class_ids::RECT_TRANSFORM,
            expected_name: class_names::RECT_TRANSFORM,
            actual_id: class.class_id(),
            actual_name: output.string(class.class_name(), "recipe class diagnostic")?,
        })
    }
}

fn vector2_value(
    value: Vector2,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<MutationValue, RecipeError> {
    let mut fields = output.vec::<MutationField>(2, "Vector2 recipe fields")?;
    fields.push(output.field("x", MutationValue::float64(value.x))?);
    fields.push(output.field("y", MutationValue::float64(value.y))?);
    Ok(MutationValue::object(fields)?)
}

fn vector3_value(
    value: Vector3,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<MutationValue, RecipeError> {
    let mut fields = output.vec::<MutationField>(3, "Vector3 recipe fields")?;
    fields.push(output.field("x", MutationValue::float64(value.x))?);
    fields.push(output.field("y", MutationValue::float64(value.y))?);
    fields.push(output.field("z", MutationValue::float64(value.z))?);
    Ok(MutationValue::object(fields)?)
}

fn quaternion_value(
    value: Quaternion,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<MutationValue, RecipeError> {
    let mut fields = output.vec::<MutationField>(4, "Quaternion recipe fields")?;
    fields.push(output.field("x", MutationValue::float64(value.x))?);
    fields.push(output.field("y", MutationValue::float64(value.y))?);
    fields.push(output.field("z", MutationValue::float64(value.z))?);
    fields.push(output.field("w", MutationValue::float64(value.w))?);
    Ok(MutationValue::object(fields)?)
}
