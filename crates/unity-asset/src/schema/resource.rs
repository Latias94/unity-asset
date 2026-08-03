use unity_asset_core::{AssetLoadBudget, class_ids, class_names};
use unity_asset_decode::media::{
    AudioClipResourceField, classify_audio_clip_resource as classify_audio_clip_resource_fields,
};

use crate::workspace::{GenericMutation, PlanPayload};

use super::recipe::{
    RecipeError, RecipeId, RecipeLowering, RecipeObject, RecipeOutputBuilder, SchemaRecipePlanner,
    SchemaVariantId, validate_recipe_provenance,
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

        let selection = classify_audio_clip_resource(audio_clip)?;
        let field_name = selection.field_name();
        let schema_variant = selection.schema_variant();
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioClipResourceSelection {
    field: AudioClipResourceField,
}

impl AudioClipResourceSelection {
    pub(crate) const fn field_name(self) -> &'static str {
        self.field.field_name()
    }

    pub(crate) const fn schema_variant(self) -> SchemaVariantId {
        match self.field {
            AudioClipResourceField::Resource => SchemaVariantId::AudioClipResource,
            AudioClipResourceField::StreamData => SchemaVariantId::AudioClipStreamDataCompatibility,
        }
    }
}

pub(crate) fn classify_audio_clip_resource(
    audio_clip: &RecipeObject,
) -> Result<AudioClipResourceSelection, RecipeError> {
    match classify_audio_clip_resource_fields(audio_clip.class().properties()) {
        Ok(Some(selection)) => Ok(AudioClipResourceSelection {
            field: selection.field(),
        }),
        Ok(None) => Err(RecipeError::UnsupportedSchema {
            variant: "AudioClip without m_Resource or compatibility m_StreamData",
        }),
        Err(source) => Err(RecipeError::InvalidMediaDescriptor { source }),
    }
}

fn validate_audio_clip(
    audio_clip: &RecipeObject,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    let class = audio_clip.class();
    if class.class_id() != class_ids::AUDIO_CLIP || class.class_name() != class_names::AUDIO_CLIP {
        return Err(RecipeError::WrongClass {
            expected_id: class_ids::AUDIO_CLIP,
            expected_name: class_names::AUDIO_CLIP,
            actual_id: class.class_id(),
            actual_name: output.string(class.class_name(), "recipe class diagnostic")?,
        });
    }
    validate_recipe_provenance(audio_clip)
}
