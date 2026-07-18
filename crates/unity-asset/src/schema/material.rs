use unity_asset_core::{FieldPath, UnityValue, class_ids, class_names};

use crate::workspace::{GenericMutation, MutationField, MutationValue, ReferenceTarget};

use super::hierarchy::Vector2;
use super::recipe::{
    RecipeError, RecipeId, RecipeLowering, RecipeObject, RecipeOutputBuilder, SchemaRecipePlanner,
    SchemaVariantId, ensure_finite, validate_recipe_provenance, validate_reference_shape,
    value_kind,
};

#[derive(Debug, Clone, PartialEq)]
pub enum MaterialTextureChange {
    Retarget {
        expected: ReferenceTarget,
        replacement: ReferenceTarget,
    },
    SetScaleOffset {
        scale: Vector2,
        offset: Vector2,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MaterialRecipe;

impl MaterialRecipe {
    pub fn lower(
        planner: &SchemaRecipePlanner<'_>,
        material: &RecipeObject,
        property_name: &str,
        change: MaterialTextureChange,
        budget: &mut unity_asset_core::AssetLoadBudget,
    ) -> Result<RecipeLowering, RecipeError> {
        planner.validate_object(material)?;
        let mut output = RecipeOutputBuilder::new(budget);
        validate_material(material, &mut output)?;
        if property_name.is_empty() || property_name.contains('\0') {
            return Err(RecipeError::PropertyNotFound {
                name: output.string(property_name, "Material property diagnostic")?,
            });
        }
        let property = find_texture_environment(material, property_name, &mut output)?;

        match change {
            MaterialTextureChange::Retarget {
                expected,
                replacement,
            } => {
                let texture_path = output.append_field(&property.value_path, "m_Texture")?;
                validate_reference_shape(material, &texture_path, &mut output)?;
                if expected == replacement {
                    return Ok(RecipeLowering::unchanged(
                        RecipeId::MaterialTextureEnvironmentV1,
                        property.variant,
                    ));
                }
                let guard = material.field_guard(&texture_path, output.budget())?;
                let action = GenericMutation::ReferenceReplace {
                    target: output.address(material.address())?,
                    path: texture_path,
                    schema_digest: guard.schema_digest(),
                    expected,
                    replacement,
                };
                let mut actions = output.vec::<GenericMutation>(1, "Material recipe actions")?;
                actions.push(action);
                RecipeLowering::changed(
                    RecipeId::MaterialTextureEnvironmentV1,
                    property.variant,
                    material.fragment(planner, Vec::new(), actions, &mut output)?,
                )
            }
            MaterialTextureChange::SetScaleOffset { scale, offset } => {
                ensure_finite(&[scale.x(), scale.y(), offset.x(), offset.y()])?;
                let scale_path = output.append_field(&property.value_path, "m_Scale")?;
                let offset_path = output.append_field(&property.value_path, "m_Offset")?;
                validate_vector2_field(material, &scale_path, &mut output)?;
                validate_vector2_field(material, &offset_path, &mut output)?;
                let scale_guard = material.field_guard(&scale_path, output.budget())?;
                let offset_guard = material.field_guard(&offset_path, output.budget())?;
                let scale_value = vector2_value(scale, &mut output)?;
                let offset_value = vector2_value(offset, &mut output)?;
                let mut actions = output.vec::<GenericMutation>(2, "Material recipe actions")?;
                actions.push(GenericMutation::FieldReplace {
                    target: output.address(material.address())?,
                    path: scale_path,
                    guard: scale_guard,
                    replacement: scale_value,
                });
                actions.push(GenericMutation::FieldReplace {
                    target: output.address(material.address())?,
                    path: offset_path,
                    guard: offset_guard,
                    replacement: offset_value,
                });
                RecipeLowering::changed(
                    RecipeId::MaterialTextureEnvironmentV1,
                    property.variant,
                    material.fragment(planner, Vec::new(), actions, &mut output)?,
                )
            }
        }
    }
}

struct MaterialProperty {
    value_path: FieldPath,
    variant: SchemaVariantId,
}

fn validate_material(
    material: &RecipeObject,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    let class = material.class();
    if class.class_id != class_ids::MATERIAL || class.class_name != class_names::MATERIAL {
        return Err(RecipeError::WrongClass {
            expected_id: class_ids::MATERIAL,
            expected_name: class_names::MATERIAL,
            actual_id: class.class_id,
            actual_name: output.string(&class.class_name, "recipe class diagnostic")?,
        });
    }
    validate_recipe_provenance(material)
}

fn find_texture_environment(
    material: &RecipeObject,
    property_name: &str,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<MaterialProperty, RecipeError> {
    let tex_envs_path = output.field_path(&["m_SavedProperties", "m_TexEnvs"])?;
    let tex_envs = material.require_field(&tex_envs_path, output)?;
    let mut found = None;
    let mut occurrences = 0_usize;
    visit_material_entries(
        tex_envs,
        &tex_envs_path,
        property_name,
        &mut occurrences,
        &mut found,
        output,
    )?;
    match (occurrences, found) {
        (0, _) => Err(RecipeError::PropertyNotFound {
            name: output.string(property_name, "Material property diagnostic")?,
        }),
        (1, Some(property)) => Ok(property),
        _ => Err(RecipeError::DuplicateProperty {
            name: output.string(property_name, "Material property diagnostic")?,
            occurrences,
        }),
    }
}

fn visit_material_entries(
    value: &UnityValue,
    path: &FieldPath,
    property_name: &str,
    occurrences: &mut usize,
    found: &mut Option<MaterialProperty>,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    match value {
        UnityValue::Array(entries) => {
            for (index, entry) in entries.iter().enumerate() {
                let entry_path = output.append_index(
                    path,
                    u32::try_from(index).map_err(|_| RecipeError::DigestLengthOverflow)?,
                )?;
                observe_material_entry(
                    entry,
                    &entry_path,
                    property_name,
                    occurrences,
                    found,
                    output,
                )?;
            }
            Ok(())
        }
        UnityValue::Object(fields) if fields.len() == 1 && fields.contains_key("data") => {
            let data_path = output.append_field(path, "data")?;
            let data = &fields["data"];
            match data {
                UnityValue::Array(_) => visit_material_entries(
                    data,
                    &data_path,
                    property_name,
                    occurrences,
                    found,
                    output,
                ),
                UnityValue::Object(_) => observe_material_entry(
                    data,
                    &data_path,
                    property_name,
                    occurrences,
                    found,
                    output,
                ),
                _ => Err(RecipeError::WrongFieldShape {
                    path: data_path,
                    expected: "a Material pair or pair array inside data",
                    actual: value_kind(data),
                }),
            }
        }
        UnityValue::Object(fields)
            if fields.contains_key("first") && fields.contains_key("second") =>
        {
            observe_material_entry(value, path, property_name, occurrences, found, output)
        }
        UnityValue::Object(fields) => {
            for (name, texture) in fields {
                let value_path = output.append_field(path, name)?;
                observe_named_material_entry(
                    name,
                    texture,
                    value_path,
                    property_name,
                    occurrences,
                    found,
                )?;
            }
            Ok(())
        }
        _ => Err(RecipeError::WrongFieldShape {
            path: output.path(path)?,
            expected: "Material texture entries, a pair array, or a legacy data wrapper",
            actual: value_kind(value),
        }),
    }
}

fn observe_material_entry(
    entry: &UnityValue,
    path: &FieldPath,
    property_name: &str,
    occurrences: &mut usize,
    found: &mut Option<MaterialProperty>,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    let (name, value, value_path, variant) = material_pair(entry, path, output)?;
    observe_material_match(
        name,
        value,
        value_path,
        variant,
        property_name,
        occurrences,
        found,
    )
}

fn observe_named_material_entry(
    name: &str,
    value: &UnityValue,
    value_path: FieldPath,
    property_name: &str,
    occurrences: &mut usize,
    found: &mut Option<MaterialProperty>,
) -> Result<(), RecipeError> {
    observe_material_match(
        name,
        value,
        value_path,
        SchemaVariantId::MaterialYamlPropertyName,
        property_name,
        occurrences,
        found,
    )
}

fn observe_material_match(
    name: &str,
    value: &UnityValue,
    value_path: FieldPath,
    variant: SchemaVariantId,
    property_name: &str,
    occurrences: &mut usize,
    found: &mut Option<MaterialProperty>,
) -> Result<(), RecipeError> {
    if name != property_name {
        return Ok(());
    }
    if !matches!(value, UnityValue::Object(_)) {
        return Err(RecipeError::WrongFieldShape {
            path: value_path,
            expected: "a UnityTexEnv object",
            actual: value_kind(value),
        });
    }
    *occurrences = occurrences
        .checked_add(1)
        .ok_or(RecipeError::DigestLengthOverflow)?;
    *found = Some(MaterialProperty {
        value_path,
        variant,
    });
    Ok(())
}

fn material_pair<'value>(
    entry: &'value UnityValue,
    path: &FieldPath,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(&'value str, &'value UnityValue, FieldPath, SchemaVariantId), RecipeError> {
    match entry {
        UnityValue::Array(values) if values.len() == 2 => {
            let variant = material_key_variant(&values[0])?;
            Ok((
                material_property_name(&values[0], path, output)?,
                &values[1],
                output.append_index(path, 1)?,
                variant,
            ))
        }
        UnityValue::Object(fields)
            if fields.len() == 2
                && fields.contains_key("first")
                && fields.contains_key("second") =>
        {
            let first = &fields["first"];
            let second = &fields["second"];
            Ok((
                material_property_name(first, path, output)?,
                second,
                output.append_field(path, "second")?,
                material_key_variant(first)?,
            ))
        }
        UnityValue::Object(fields) if fields.len() == 1 => {
            let (name, value) = fields.first().ok_or(RecipeError::UnsupportedSchema {
                variant: "empty Material property entry",
            })?;
            Ok((
                name,
                value,
                output.append_field(path, name)?,
                SchemaVariantId::MaterialYamlPropertyName,
            ))
        }
        _ => Err(RecipeError::WrongFieldShape {
            path: output.path(path)?,
            expected: "a two-element pair",
            actual: value_kind(entry),
        }),
    }
}

fn material_key_variant(key: &UnityValue) -> Result<SchemaVariantId, RecipeError> {
    match key {
        UnityValue::String(_) => Ok(SchemaVariantId::MaterialStringPropertyName),
        UnityValue::Object(fields) if matches!(fields.get("name"), Some(UnityValue::String(_))) => {
            Ok(SchemaVariantId::MaterialFastPropertyName)
        }
        _ => Err(RecipeError::UnsupportedSchema {
            variant: "material texture property key",
        }),
    }
}

fn material_property_name<'value>(
    key: &'value UnityValue,
    path: &FieldPath,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<&'value str, RecipeError> {
    match key {
        UnityValue::String(value) => Ok(value),
        UnityValue::Object(fields) => {
            if let Some(name) = fields.get("name").and_then(UnityValue::as_str) {
                Ok(name)
            } else {
                Err(RecipeError::WrongFieldShape {
                    path: output.path(path)?,
                    expected: "a string or FastPropertyName key",
                    actual: value_kind(key),
                })
            }
        }
        _ => Err(RecipeError::WrongFieldShape {
            path: output.path(path)?,
            expected: "a string or FastPropertyName key",
            actual: value_kind(key),
        }),
    }
}

fn validate_vector2_field(
    material: &RecipeObject,
    path: &FieldPath,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    let value = material.require_field(path, output)?;
    let Some(fields) = value.as_object() else {
        return Err(RecipeError::WrongFieldShape {
            path: output.path(path)?,
            expected: "a Vector2 object",
            actual: value_kind(value),
        });
    };
    if fields.get("x").and_then(UnityValue::as_f64).is_none()
        || fields.get("y").and_then(UnityValue::as_f64).is_none()
    {
        return Err(RecipeError::WrongFieldShape {
            path: output.path(path)?,
            expected: "a Vector2 object with numeric x/y",
            actual: value_kind(value),
        });
    }
    Ok(())
}

fn vector2_value(
    value: Vector2,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<MutationValue, RecipeError> {
    let mut fields = output.vec::<MutationField>(2, "Material Vector2 fields")?;
    fields.push(output.field("x", MutationValue::float64(value.x()))?);
    fields.push(output.field("y", MutationValue::float64(value.y()))?);
    Ok(MutationValue::object(fields)?)
}
