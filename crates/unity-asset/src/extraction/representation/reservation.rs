//! Working-set proof for private extraction representations.

use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError, ObjectAddress};
use unity_asset_yaml::UnityYamlSerializer;

#[cfg(feature = "decode")]
use unity_asset_binary::asset::class_ids;
#[cfg(feature = "decode")]
use unity_asset_decode::{
    audio::{AudioClipLayout, AudioCompressionFormat, MAX_VORBIS_SETUP_PACKET_BYTES},
    descriptor::{MediaDescriptor, MediaFamily},
    media::StreamDataRef,
    sprite::{SpriteLayout, SpriteTextureReference},
    texture::{PreparedTexturePng, Texture2DLayout},
};

use super::super::{CheckedByteCounter, source_budget_error};
#[cfg(feature = "decode")]
use super::contract::PlannedStreamSource;
use super::contract::{PlannedContent, RepresentationContract};
#[cfg(feature = "decode")]
use super::texture_inspection_context;
#[cfg(feature = "decode")]
use crate::reference::{ReferenceGraphError, binary_external_source_resolves_to};
#[cfg(feature = "decode")]
use crate::workspace::{StreamedResourceResolution, StreamedResourceResolver};
use crate::workspace::{
    WorkspaceError, WorkspaceLookup, WorkspaceObject, WorkspaceObjectValue, WorkspaceView,
};

#[derive(Debug, Error)]
pub(in crate::extraction) enum ExtractionReservationError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[cfg(feature = "decode")]
    #[error(transparent)]
    Reference(#[from] ReferenceGraphError),
    #[error("planned extraction object is unavailable: {0:?}")]
    ObjectUnavailable(ObjectAddress),
    #[error("planned extraction content is inconsistent with the workspace: {0}")]
    ContentMismatch(&'static str),
    #[error("arithmetic overflow while proving {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("failed to measure canonical YAML output: {0}")]
    YamlSizing(String),
}

pub(in crate::extraction) fn trusted_working_set(
    view: &dyn WorkspaceView,
    address: &ObjectAddress,
    contract: &RepresentationContract,
    #[cfg(feature = "decode")] stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
    budget: &mut AssetLoadBudget,
) -> Result<u64, ExtractionReservationError> {
    contract.validate_current_semantics().map_err(|_| {
        ExtractionReservationError::ContentMismatch(
            "planned representation semantics do not match the current implementation",
        )
    })?;
    let object = read_object(view, address, budget)?;
    let preferred = content_working_set(
        view,
        &object,
        contract.preferred_content(),
        #[cfg(feature = "decode")]
        stream_resolver,
        budget,
    )?;
    let fallback = contract
        .fallback()
        .map(|_| raw_binary_working_set(&object))
        .transpose()?
        .unwrap_or(0);
    Ok(preferred.max(fallback).max(1))
}

pub(in crate::extraction) fn raw_binary_working_set(
    object: &WorkspaceObject,
) -> Result<u64, ExtractionReservationError> {
    let WorkspaceObjectValue::Binary(binary) = object.value() else {
        return Err(ExtractionReservationError::ContentMismatch(
            "binary content was planned for a YAML object",
        ));
    };
    usize_to_u64(binary.payload_len(), "raw binary working set")
}

pub(in crate::extraction) fn yaml_working_set(
    object: &WorkspaceObject,
    budget: &mut AssetLoadBudget,
) -> Result<u64, ExtractionReservationError> {
    if !matches!(object.value(), WorkspaceObjectValue::Yaml(_)) {
        return Err(ExtractionReservationError::ContentMismatch(
            "YAML content was planned for a binary object",
        ));
    }
    let mut counter = CheckedByteCounter::new("planned YAML output length overflow");
    if let Err(error) = UnityYamlSerializer::new().serialize_to_writer_with_budget(
        &mut counter,
        std::iter::once(object.class()),
        budget,
    ) {
        if let Some(error) = source_budget_error(&error) {
            return Err(error.clone().into());
        }
        return Err(ExtractionReservationError::YamlSizing(error.to_string()));
    }
    Ok(counter.bytes().max(1))
}

fn content_working_set(
    _view: &dyn WorkspaceView,
    object: &WorkspaceObject,
    content: &PlannedContent,
    #[cfg(feature = "decode")] stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
    budget: &mut AssetLoadBudget,
) -> Result<u64, ExtractionReservationError> {
    match content {
        PlannedContent::RawBinary | PlannedContent::TextAsset => raw_binary_working_set(object),
        PlannedContent::Yaml => yaml_working_set(object, budget),
        #[cfg(feature = "decode")]
        PlannedContent::Audio { stream, descriptor } => audio_working_set(
            _view,
            object,
            descriptor,
            stream.as_ref(),
            stream_resolver,
            budget,
        ),
        #[cfg(feature = "decode")]
        PlannedContent::TexturePng { stream, descriptor } => texture_working_set(
            _view,
            object,
            descriptor,
            stream.as_ref(),
            stream_resolver,
            budget,
        ),
        #[cfg(feature = "decode")]
        PlannedContent::SpritePng {
            texture,
            texture_stream,
            descriptor,
        } => SpriteExecutionProof::verify(
            _view,
            object,
            PlannedSpriteExecution {
                texture,
                texture_stream: texture_stream.as_ref(),
                descriptor,
            },
            stream_resolver,
            budget,
        )
        .map(SpriteExecutionProof::working_set_bytes),
        #[cfg(not(feature = "decode"))]
        PlannedContent::Audio { .. }
        | PlannedContent::TexturePng { .. }
        | PlannedContent::SpritePng { .. } => raw_binary_working_set(object),
    }
}

#[cfg(feature = "decode")]
pub(in crate::extraction) fn audio_working_set(
    view: &dyn WorkspaceView,
    object: &WorkspaceObject,
    descriptor: &MediaDescriptor,
    stream: Option<&PlannedStreamSource>,
    stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
    budget: &mut AssetLoadBudget,
) -> Result<u64, ExtractionReservationError> {
    let binary = binary_with_class(object, class_ids::AUDIO_CLIP, "AudioClip")?;
    let layout = AudioClipLayout::inspect(binary).map_err(|_| {
        ExtractionReservationError::ContentMismatch("AudioClip layout can no longer be inspected")
    })?;
    if descriptor.family() != MediaFamily::Audio {
        return Err(ExtractionReservationError::ContentMismatch(
            "AudioClip descriptor has the wrong media family",
        ));
    }
    validate_payload_request(
        view,
        object,
        layout.payload().stream(),
        stream,
        stream_resolver,
        budget,
    )?;
    let encoded_bytes = stream.map_or_else(
        || {
            layout
                .payload()
                .embedded_byte_len()
                .ok_or(ExtractionReservationError::ContentMismatch(
                    "embedded AudioClip payload is unavailable",
                ))
                .and_then(|length| usize_to_u64(length, "embedded audio size"))
        },
        |source| Ok(source.request().size()),
    )?;
    if descriptor.input_bytes() != encoded_bytes {
        return Err(ExtractionReservationError::ContentMismatch(
            "AudioClip descriptor input length changed",
        ));
    }
    let output_bound = if layout.compression_format() == AudioCompressionFormat::Vorbis {
        ogg_output_bound(encoded_bytes)?
    } else {
        encoded_bytes
    };
    checked_sum(
        [
            usize_to_u64(binary.payload_len(), "audio working set")?,
            stream.map_or(0, |source| source.request().size()),
            if stream.is_none() { encoded_bytes } else { 0 },
            output_bound,
        ],
        "audio working set",
    )
}

#[cfg(feature = "decode")]
pub(in crate::extraction) fn texture_working_set(
    view: &dyn WorkspaceView,
    object: &WorkspaceObject,
    descriptor: &MediaDescriptor,
    stream: Option<&PlannedStreamSource>,
    stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
    budget: &mut AssetLoadBudget,
) -> Result<u64, ExtractionReservationError> {
    let binary = binary_with_class(object, class_ids::TEXTURE_2D, "Texture2D")?;
    let context = texture_inspection_context(view, object)?;
    let layout = Texture2DLayout::inspect(binary, context).map_err(|_| {
        ExtractionReservationError::ContentMismatch("Texture2D layout can no longer be inspected")
    })?;
    validate_texture_descriptor(layout, descriptor, MediaFamily::Texture)?;
    validate_payload_request(
        view,
        object,
        layout.payload().stream(),
        stream,
        stream_resolver,
        budget,
    )?;
    image_working_set(binary.payload_len(), layout, stream)
}

#[cfg(feature = "decode")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SpriteExecutionProof {
    working_set_bytes: u64,
}

#[cfg(feature = "decode")]
#[derive(Debug, Clone, Copy)]
struct PlannedSpriteExecution<'plan> {
    texture: &'plan ObjectAddress,
    texture_stream: Option<&'plan PlannedStreamSource>,
    descriptor: &'plan MediaDescriptor,
}

#[cfg(feature = "decode")]
impl SpriteExecutionProof {
    fn verify(
        view: &dyn WorkspaceView,
        object: &WorkspaceObject,
        planned: PlannedSpriteExecution<'_>,
        stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ExtractionReservationError> {
        let sprite = binary_with_class(object, class_ids::SPRITE, "Sprite")?;
        let sprite_layout = SpriteLayout::inspect(sprite).map_err(|_| {
            ExtractionReservationError::ContentMismatch("Sprite layout can no longer be inspected")
        })?;
        let texture_reference = sprite_layout.texture();
        let texture_object = read_object(view, planned.texture, budget)?;
        if !sprite_texture_address_matches(
            view,
            object,
            texture_reference,
            planned.texture,
            &texture_object,
            budget,
        )? {
            return Err(ExtractionReservationError::ContentMismatch(
                "Sprite texture address does not match its current PPtr",
            ));
        }
        let working_set_bytes = sprite_working_set_with_texture(
            view,
            object,
            &texture_object,
            planned.descriptor,
            planned.texture_stream,
            stream_resolver,
            budget,
        )?;
        Ok(Self { working_set_bytes })
    }

    const fn working_set_bytes(self) -> u64 {
        self.working_set_bytes
    }
}

#[cfg(feature = "decode")]
fn sprite_texture_address_matches(
    view: &dyn WorkspaceView,
    sprite: &WorkspaceObject,
    reference: SpriteTextureReference,
    planned: &ObjectAddress,
    texture: &WorkspaceObject,
    budget: &mut AssetLoadBudget,
) -> Result<bool, ExtractionReservationError> {
    if planned.binary_path_id() != Some(reference.path_id())
        || texture.handle().object().binary_path_id() != Some(reference.path_id())
    {
        return Ok(false);
    }
    if reference.file_id() == 0 {
        let owner = match view.source(sprite.handle().object().source(), budget)? {
            WorkspaceLookup::Resolved(source) => source,
            WorkspaceLookup::Unloaded
            | WorkspaceLookup::Missing
            | WorkspaceLookup::Ambiguous { .. }
            | WorkspaceLookup::Invalid { .. } => {
                return Err(ExtractionReservationError::ContentMismatch(
                    "Sprite source provenance is unavailable",
                ));
            }
        };
        return Ok(texture.handle().object().source() == owner.id()
            && planned.source_locator() == owner.locator());
    }
    Ok(binary_external_source_resolves_to(
        view,
        sprite.handle().object().source(),
        reference.file_id(),
        texture.handle().object().source(),
        budget,
    )?)
}

#[cfg(feature = "decode")]
pub(in crate::extraction) fn sprite_working_set_with_texture(
    view: &dyn WorkspaceView,
    object: &WorkspaceObject,
    texture_object: &WorkspaceObject,
    descriptor: &MediaDescriptor,
    texture_stream: Option<&PlannedStreamSource>,
    stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
    budget: &mut AssetLoadBudget,
) -> Result<u64, ExtractionReservationError> {
    let sprite = binary_with_class(object, class_ids::SPRITE, "Sprite")?;
    let sprite_layout = SpriteLayout::inspect(sprite).map_err(|_| {
        ExtractionReservationError::ContentMismatch("Sprite layout can no longer be inspected")
    })?;
    let texture = binary_with_class(texture_object, class_ids::TEXTURE_2D, "Sprite Texture2D")?;
    let context = texture_inspection_context(view, texture_object)?;
    let layout = Texture2DLayout::inspect(texture, context).map_err(|_| {
        ExtractionReservationError::ContentMismatch(
            "Sprite Texture2D layout can no longer be inspected",
        )
    })?;
    validate_texture_descriptor(layout, descriptor, MediaFamily::Sprite)?;
    let dimensions = descriptor
        .dimensions()
        .ok_or(ExtractionReservationError::ContentMismatch(
            "Sprite descriptor no longer has dimensions",
        ))?;
    if dimensions.width() != sprite_layout.rect().width()
        || dimensions.height() != sprite_layout.rect().height()
    {
        return Err(ExtractionReservationError::ContentMismatch(
            "Sprite descriptor dimensions no longer match its strict layout",
        ));
    }
    validate_payload_request(
        view,
        texture_object,
        layout.payload().stream(),
        texture_stream,
        stream_resolver,
        budget,
    )?;
    let cropped_rgba_bytes = u64::from(dimensions.width())
        .checked_mul(u64::from(dimensions.height()))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ExtractionReservationError::ArithmeticOverflow {
            resource: "sprite cropped RGBA working set",
        })?;
    checked_sum(
        [
            usize_to_u64(sprite.payload_len(), "sprite working set")?,
            image_working_set(texture.payload_len(), layout, texture_stream)?,
            cropped_rgba_bytes,
        ],
        "sprite working set",
    )
}

#[cfg(feature = "decode")]
fn image_working_set(
    binary_bytes: usize,
    layout: Texture2DLayout<'_>,
    stream: Option<&PlannedStreamSource>,
) -> Result<u64, ExtractionReservationError> {
    let image_bytes = u64::from(layout.width())
        .checked_mul(u64::from(layout.height()))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ExtractionReservationError::ArithmeticOverflow {
            resource: "texture image working set",
        })?;
    let embedded_bytes = layout
        .payload()
        .embedded_byte_len()
        .map(|length| usize_to_u64(length, "embedded texture size"))
        .transpose()?
        .unwrap_or(0);
    checked_sum(
        [
            image_bytes,
            png_output_bound(image_bytes)?,
            layout.platform_copy_bytes(),
            usize_to_u64(binary_bytes, "texture binary working set")?,
            stream.map_or(0, |source| source.request().size()),
            embedded_bytes,
        ],
        "texture working set",
    )
}

#[cfg(feature = "decode")]
fn binary_with_class<'object>(
    object: &'object WorkspaceObject,
    expected_class: i32,
    name: &'static str,
) -> Result<&'object unity_asset_binary::object::UnityObject, ExtractionReservationError> {
    let WorkspaceObjectValue::Binary(binary) = object.value() else {
        return Err(ExtractionReservationError::ContentMismatch(
            "decoded binary content was planned for a YAML object",
        ));
    };
    if binary.class_id() != expected_class {
        return Err(ExtractionReservationError::ContentMismatch(name));
    }
    Ok(binary)
}

#[cfg(feature = "decode")]
fn validate_payload_request(
    view: &dyn WorkspaceView,
    owner_object: &WorkspaceObject,
    expected: Option<StreamDataRef<'_>>,
    planned: Option<&PlannedStreamSource>,
    stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
    budget: &mut AssetLoadBudget,
) -> Result<(), ExtractionReservationError> {
    match (expected, planned) {
        (None, None) => Ok(()),
        (Some(expected), Some(source))
            if expected.path() == source.request().stream_path()
                && expected.offset() == source.request().offset()
                && expected.size() == source.request().size() =>
        {
            validate_resolved_stream(view, owner_object, source, stream_resolver, budget)
        }
        _ => Err(ExtractionReservationError::ContentMismatch(
            "streamed payload range does not match inspected media metadata",
        )),
    }
}

#[cfg(feature = "decode")]
fn validate_resolved_stream(
    view: &dyn WorkspaceView,
    owner_object: &WorkspaceObject,
    planned: &PlannedStreamSource,
    stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
    budget: &mut AssetLoadBudget,
) -> Result<(), ExtractionReservationError> {
    let request = planned.request();
    let owner_id = owner_object.handle().object().source();
    let owner = match view.source(owner_id, budget)? {
        WorkspaceLookup::Resolved(source) => source,
        WorkspaceLookup::Unloaded
        | WorkspaceLookup::Missing
        | WorkspaceLookup::Ambiguous { .. }
        | WorkspaceLookup::Invalid { .. } => {
            return Err(ExtractionReservationError::ContentMismatch(
                "streamed payload owner is unavailable",
            ));
        }
    };
    if owner.locator() != request.owner() {
        return Err(ExtractionReservationError::ContentMismatch(
            "streamed payload owner no longer matches the planned request",
        ));
    }
    let stream_resolver = stream_resolver.ok_or(ExtractionReservationError::ContentMismatch(
        "streamed payload resolver is unavailable",
    ))?;
    let StreamedResourceResolution::Resolved { resource } =
        stream_resolver.resolve_request(request, budget)?
    else {
        return Err(ExtractionReservationError::ContentMismatch(
            "streamed payload no longer resolves uniquely",
        ));
    };
    if !planned.matches_resolution(&resource) {
        return Err(ExtractionReservationError::ContentMismatch(
            "planned streamed source is not the canonical request resolution",
        ));
    }
    planned.open(view, budget)?;
    Ok(())
}

#[cfg(feature = "decode")]
fn validate_texture_descriptor(
    layout: Texture2DLayout<'_>,
    descriptor: &MediaDescriptor,
    family: MediaFamily,
) -> Result<(), ExtractionReservationError> {
    if descriptor.family() != family
        || descriptor.input_bytes() != layout.complete_image_size()
        || descriptor.texture_encoding() != layout.format().descriptor_encoding()
    {
        return Err(ExtractionReservationError::ContentMismatch(
            "texture descriptor no longer matches its strict layout",
        ));
    }
    if family == MediaFamily::Texture {
        let dimensions =
            descriptor
                .dimensions()
                .ok_or(ExtractionReservationError::ContentMismatch(
                    "texture descriptor no longer has dimensions",
                ))?;
        if dimensions.width() != layout.width() || dimensions.height() != layout.height() {
            return Err(ExtractionReservationError::ContentMismatch(
                "texture descriptor dimensions no longer match its strict layout",
            ));
        }
    }
    Ok(())
}

fn read_object(
    view: &dyn WorkspaceView,
    address: &ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceObject, ExtractionReservationError> {
    let handle = match view.resolve_object(address, budget)? {
        WorkspaceLookup::Resolved(handle) => handle,
        WorkspaceLookup::Unloaded
        | WorkspaceLookup::Missing
        | WorkspaceLookup::Ambiguous { .. }
        | WorkspaceLookup::Invalid { .. } => {
            return Err(ExtractionReservationError::ObjectUnavailable(
                address.clone(),
            ));
        }
    };
    view.read_object(&handle, budget).map_err(Into::into)
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, ExtractionReservationError> {
    u64::try_from(value).map_err(|_| ExtractionReservationError::ArithmeticOverflow { resource })
}

#[cfg(feature = "decode")]
fn checked_sum(
    values: impl IntoIterator<Item = u64>,
    resource: &'static str,
) -> Result<u64, ExtractionReservationError> {
    values.into_iter().try_fold(0_u64, |total, value| {
        total
            .checked_add(value)
            .ok_or(ExtractionReservationError::ArithmeticOverflow { resource })
    })
}

#[cfg(feature = "decode")]
fn png_output_bound(rgba_bytes: u64) -> Result<u64, ExtractionReservationError> {
    PreparedTexturePng::output_bound_for_rgba(rgba_bytes).map_err(|_| {
        ExtractionReservationError::ArithmeticOverflow {
            resource: "PNG output bound",
        }
    })
}

#[cfg(feature = "decode")]
fn ogg_output_bound(encoded_bytes: u64) -> Result<u64, ExtractionReservationError> {
    encoded_bytes
        .checked_mul(16)
        .and_then(|bytes| {
            MAX_VORBIS_SETUP_PACKET_BYTES
                .checked_mul(2)
                .and_then(|fixed| bytes.checked_add(fixed))
        })
        .and_then(|bytes| bytes.checked_add(64 * 1024))
        .ok_or(ExtractionReservationError::ArithmeticOverflow {
            resource: "Ogg output bound",
        })
}
