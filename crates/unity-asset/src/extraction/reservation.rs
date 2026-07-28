use thiserror::Error;
#[cfg(feature = "decode")]
use unity_asset_core::SourceLocator;
use unity_asset_core::{AssetLoadBudget, BudgetError, ObjectAddress};
use unity_asset_yaml::UnityYamlSerializer;

#[cfg(feature = "decode")]
use unity_asset_binary::asset::class_ids;
#[cfg(feature = "decode")]
use unity_asset_decode::{
    audio::{AudioClipLayout, AudioCompressionFormat, MAX_VORBIS_SETUP_PACKET_BYTES},
    media::StreamDataRef,
    sprite::SpriteTextureReference,
    texture::Texture2DLayout,
};

#[cfg(feature = "decode")]
use super::model::ExtractionSourceRange;
use super::model::{PlannedArtifact, PlannedContent};
use super::{CheckedByteCounter, source_budget_error};
#[cfg(feature = "decode")]
use crate::reference::{ReferenceGraphError, binary_external_source_resolves_to};
#[cfg(feature = "decode")]
use crate::workspace::{StreamedResourceResolution, StreamedResourceResolver};
use crate::workspace::{
    WorkspaceError, WorkspaceLookup, WorkspaceObject, WorkspaceObjectValue, WorkspaceView,
};

#[derive(Debug, Error)]
pub(super) enum ExtractionReservationError {
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
    #[error(
        "streamed extraction range {offset}..{end} exceeds source {locator:?} length {source_len}"
    )]
    #[cfg(feature = "decode")]
    StreamOutOfRange {
        locator: SourceLocator,
        offset: u64,
        end: u64,
        source_len: u64,
    },
    #[error("arithmetic overflow while proving {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("failed to measure canonical YAML output: {0}")]
    YamlSizing(String),
}

pub(super) fn trusted_working_set(
    view: &dyn WorkspaceView,
    artifact: &PlannedArtifact,
    #[cfg(feature = "decode")] stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
    budget: &mut AssetLoadBudget,
) -> Result<u64, ExtractionReservationError> {
    let object = read_object(view, artifact.address(), budget)?;
    let preferred = content_working_set(
        view,
        &object,
        artifact.preferred_content(),
        #[cfg(feature = "decode")]
        stream_resolver,
        budget,
    )?;
    let fallback = artifact
        .fallback_content()
        .map(|content| {
            content_working_set(
                view,
                &object,
                content,
                #[cfg(feature = "decode")]
                stream_resolver,
                budget,
            )
        })
        .transpose()?
        .unwrap_or(0);
    Ok(preferred.max(fallback).max(1))
}

#[cfg(feature = "decode")]
pub(super) fn requires_stream_resolution(artifact: &PlannedArtifact) -> bool {
    artifact.preferred_content().stream_range().is_some()
        || artifact
            .fallback_content()
            .and_then(PlannedContent::stream_range)
            .is_some()
}

pub(super) fn raw_binary_working_set(
    object: &WorkspaceObject,
) -> Result<u64, ExtractionReservationError> {
    let WorkspaceObjectValue::Binary(binary) = object.value() else {
        return Err(ExtractionReservationError::ContentMismatch(
            "binary content was planned for a YAML object",
        ));
    };
    usize_to_u64(binary.payload_len(), "raw binary working set")
}

pub(super) fn yaml_working_set(
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
        PlannedContent::Audio {
            version,
            extension,
            stream,
        } => audio_working_set(
            _view,
            object,
            version,
            extension,
            stream.as_ref(),
            stream_resolver,
            budget,
        ),
        #[cfg(feature = "decode")]
        PlannedContent::TexturePng { stream, .. } => {
            texture_working_set(_view, object, stream.as_ref(), stream_resolver, budget)
        }
        #[cfg(feature = "decode")]
        PlannedContent::SpritePng {
            texture,
            texture_stream,
        } => SpriteExecutionProof::verify(
            _view,
            object,
            PlannedSpriteExecution {
                texture,
                texture_stream: texture_stream.as_ref(),
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
pub(super) fn audio_working_set(
    view: &dyn WorkspaceView,
    object: &WorkspaceObject,
    version: &unity_asset_binary::unity_version::UnityVersion,
    extension: &str,
    stream: Option<&ExtractionSourceRange>,
    stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
    budget: &mut AssetLoadBudget,
) -> Result<u64, ExtractionReservationError> {
    let binary = binary_with_class(object, class_ids::AUDIO_CLIP, "AudioClip")?;
    let layout = AudioClipLayout::inspect(binary, version).map_err(|_| {
        ExtractionReservationError::ContentMismatch("AudioClip layout can no longer be inspected")
    })?;
    if extension != layout.compression_format().extension() {
        return Err(ExtractionReservationError::ContentMismatch(
            "AudioClip extension does not match its codec",
        ));
    }
    validate_payload_range(
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
        |range| Ok(range.size()),
    )?;
    let output_bound = if layout.compression_format() == AudioCompressionFormat::Vorbis {
        ogg_output_bound(encoded_bytes)?
    } else {
        encoded_bytes
    };
    checked_sum(
        [
            usize_to_u64(binary.payload_len(), "audio working set")?,
            stream.map_or(0, ExtractionSourceRange::size),
            if stream.is_none() { encoded_bytes } else { 0 },
            output_bound,
        ],
        "audio working set",
    )
}

#[cfg(feature = "decode")]
pub(super) fn texture_working_set(
    view: &dyn WorkspaceView,
    object: &WorkspaceObject,
    stream: Option<&ExtractionSourceRange>,
    stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
    budget: &mut AssetLoadBudget,
) -> Result<u64, ExtractionReservationError> {
    let binary = binary_with_class(object, class_ids::TEXTURE_2D, "Texture2D")?;
    let layout = Texture2DLayout::inspect(binary).map_err(|_| {
        ExtractionReservationError::ContentMismatch("Texture2D layout can no longer be inspected")
    })?;
    validate_payload_range(
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
    texture_stream: Option<&'plan ExtractionSourceRange>,
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
        let texture_reference = SpriteTextureReference::inspect(sprite).map_err(|_| {
            ExtractionReservationError::ContentMismatch(
                "Sprite texture reference can no longer be inspected",
            )
        })?;
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
pub(super) fn sprite_working_set_with_texture(
    view: &dyn WorkspaceView,
    object: &WorkspaceObject,
    texture_object: &WorkspaceObject,
    texture_stream: Option<&ExtractionSourceRange>,
    stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
    budget: &mut AssetLoadBudget,
) -> Result<u64, ExtractionReservationError> {
    let sprite = binary_with_class(object, class_ids::SPRITE, "Sprite")?;
    let texture = binary_with_class(texture_object, class_ids::TEXTURE_2D, "Sprite Texture2D")?;
    let layout = Texture2DLayout::inspect(texture).map_err(|_| {
        ExtractionReservationError::ContentMismatch(
            "Sprite Texture2D layout can no longer be inspected",
        )
    })?;
    validate_payload_range(
        view,
        texture_object,
        layout.payload().stream(),
        texture_stream,
        stream_resolver,
        budget,
    )?;
    checked_sum(
        [
            usize_to_u64(sprite.payload_len(), "sprite working set")?,
            image_working_set(texture.payload_len(), layout, texture_stream)?,
        ],
        "sprite working set",
    )
}

#[cfg(feature = "decode")]
fn image_working_set(
    binary_bytes: usize,
    layout: Texture2DLayout<'_>,
    stream: Option<&ExtractionSourceRange>,
) -> Result<u64, ExtractionReservationError> {
    let image_bytes = u64::try_from(layout.width())
        .ok()
        .and_then(|width| {
            u64::try_from(layout.height())
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
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
            usize_to_u64(binary_bytes, "texture binary working set")?,
            stream.map_or(0, ExtractionSourceRange::size),
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
fn validate_payload_range(
    view: &dyn WorkspaceView,
    owner_object: &WorkspaceObject,
    expected: Option<StreamDataRef<'_>>,
    planned: Option<&ExtractionSourceRange>,
    stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
    budget: &mut AssetLoadBudget,
) -> Result<(), ExtractionReservationError> {
    match (expected, planned) {
        (None, None) => Ok(()),
        (Some(expected), Some(range))
            if expected.offset() == range.offset()
                && u64::from(expected.size()) == range.size() =>
        {
            if let Some(stream_resolver) = stream_resolver {
                validate_resolved_stream(
                    view,
                    owner_object,
                    expected,
                    range,
                    stream_resolver,
                    budget,
                )?;
            }
            validate_source_range(view, range, budget)
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
    expected: StreamDataRef<'_>,
    planned: &ExtractionSourceRange,
    stream_resolver: &StreamedResourceResolver<'_, '_>,
    budget: &mut AssetLoadBudget,
) -> Result<(), ExtractionReservationError> {
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
    let resolution = stream_resolver.resolve(
        &owner,
        expected.path(),
        expected.offset(),
        u64::from(expected.size()),
        budget,
    )?;
    let StreamedResourceResolution::Resolved { resource } = resolution else {
        return Err(ExtractionReservationError::ContentMismatch(
            "streamed payload no longer resolves uniquely",
        ));
    };
    if resource.source().locator() != planned.source()
        || resource.offset() != planned.offset()
        || resource.size() != planned.size()
    {
        return Err(ExtractionReservationError::ContentMismatch(
            "streamed payload source does not match inspected media metadata",
        ));
    }
    Ok(())
}

#[cfg(feature = "decode")]
fn validate_source_range(
    view: &dyn WorkspaceView,
    range: &ExtractionSourceRange,
    budget: &mut AssetLoadBudget,
) -> Result<(), ExtractionReservationError> {
    let source = match view.resolve_source(range.source(), budget)? {
        WorkspaceLookup::Resolved(source) => source,
        WorkspaceLookup::Unloaded
        | WorkspaceLookup::Missing
        | WorkspaceLookup::Ambiguous { .. }
        | WorkspaceLookup::Invalid { .. } => {
            return Err(ExtractionReservationError::ContentMismatch(
                "streamed payload source is unavailable",
            ));
        }
    };
    let source_len = view.source_length(source.id())?;
    let end = range.end();
    if end > source_len {
        return Err(ExtractionReservationError::StreamOutOfRange {
            locator: range.source().clone(),
            offset: range.offset(),
            end,
            source_len,
        });
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
    rgba_bytes
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(1024 * 1024))
        .ok_or(ExtractionReservationError::ArithmeticOverflow {
            resource: "PNG output bound",
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
