use unity_asset_core::{AssetLoadBudget, UnityValue, class_ids, class_names};

use crate::workspace::{GenericMutation, PlanPayload};

use super::recipe::{
    RecipeError, RecipeId, RecipeLowering, RecipeObject, RecipeOutputBuilder, SchemaRecipePlanner,
    SchemaVariantId, validate_recipe_provenance, value_kind,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct AudioClipResourceRecipe;

impl AudioClipResourceRecipe {
    pub fn lower(
        planner: &SchemaRecipePlanner<'_>,
        audio_clip: &RecipeObject,
        payload: PlanPayload,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecipeLowering, RecipeError> {
        planner.validate_object(audio_clip)?;
        let mut output = RecipeOutputBuilder::new(budget);
        validate_audio_clip(audio_clip, &mut output)?;
        if payload.bytes().is_empty() {
            return Err(RecipeError::InvalidPayload {
                reason: "streamed AudioClip payload must not be empty",
            });
        }

        let resource = classify_resource_field(
            audio_clip,
            "m_Resource",
            ResourceShape::Resource,
            &mut output,
        )?;
        let stream_data = classify_resource_field(
            audio_clip,
            "m_StreamData",
            ResourceShape::StreamData,
            &mut output,
        )?;
        let (field_name, schema_variant) = match (resource, stream_data) {
            (Candidate::Valid, _) => ("m_Resource", SchemaVariantId::AudioClipResource),
            (Candidate::Absent, Candidate::Valid) => (
                "m_StreamData",
                SchemaVariantId::AudioClipStreamDataCompatibility,
            ),
            (Candidate::Absent, Candidate::Absent) => {
                return Err(RecipeError::UnsupportedSchema {
                    variant: "AudioClip without m_Resource or compatibility m_StreamData",
                });
            }
        };
        if schema_variant == SchemaVariantId::AudioClipStreamDataCompatibility
            && payload.bytes().len() > u32::MAX as usize
        {
            return Err(RecipeError::InvalidPayload {
                reason: "m_StreamData compatibility payload exceeds its u32 size domain",
            });
        }

        let path = output.field_path(&[field_name])?;
        let guard = audio_clip.field_guard(&path, output.budget())?;
        let digest = payload.digest();
        let action = GenericMutation::ResourceReplace {
            target: output.address(audio_clip.address())?,
            path,
            guard,
            payload: digest,
        };
        let mut payloads = output.vec::<PlanPayload>(1, "AudioClip recipe payloads")?;
        payloads.push(payload);
        let mut actions = output.vec::<GenericMutation>(1, "AudioClip recipe actions")?;
        actions.push(action);
        RecipeLowering::changed(
            RecipeId::AudioClipStreamedResourceV1,
            schema_variant,
            audio_clip.fragment(planner, payloads, actions, &mut output)?,
        )
    }
}

#[derive(Clone, Copy)]
enum ResourceShape {
    Resource,
    StreamData,
}

#[derive(Clone, Copy)]
enum Candidate {
    Absent,
    Valid,
}

fn validate_audio_clip(
    audio_clip: &RecipeObject,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    let class = audio_clip.class();
    if class.class_id != class_ids::AUDIO_CLIP || class.class_name != class_names::AUDIO_CLIP {
        return Err(RecipeError::WrongClass {
            expected_id: class_ids::AUDIO_CLIP,
            expected_name: class_names::AUDIO_CLIP,
            actual_id: class.class_id,
            actual_name: output.string(&class.class_name, "recipe class diagnostic")?,
        });
    }
    validate_recipe_provenance(audio_clip)
}

fn classify_resource_field(
    audio_clip: &RecipeObject,
    field_name: &'static str,
    shape: ResourceShape,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<Candidate, RecipeError> {
    let path = output.field_path(&[field_name])?;
    let Some(value) = audio_clip.field(&path) else {
        return Ok(Candidate::Absent);
    };
    let Some(fields) = value.as_object() else {
        return Err(RecipeError::WrongFieldShape {
            path,
            expected: "a streamed-resource object",
            actual: value_kind(value),
        });
    };
    let (source_name, offset_name, size_name) = match shape {
        ResourceShape::Resource => ("m_Source", "m_Offset", "m_Size"),
        ResourceShape::StreamData => ("path", "offset", "size"),
    };
    let source_valid = matches!(fields.get(source_name), Some(UnityValue::String(_)));
    let offset_valid = fields
        .get(offset_name)
        .and_then(UnityValue::as_u64)
        .is_some();
    let size_valid = fields.get(size_name).and_then(UnityValue::as_u64).is_some();
    if !source_valid || !offset_valid || !size_valid {
        return Err(RecipeError::WrongFieldShape {
            path,
            expected: match shape {
                ResourceShape::Resource => "m_Source:string, m_Offset:unsigned, m_Size:unsigned",
                ResourceShape::StreamData => "path:string, offset:unsigned, size:unsigned",
            },
            actual: value_kind(value),
        });
    }
    Ok(Candidate::Valid)
}
