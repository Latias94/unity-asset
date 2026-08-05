//! Execution-time preparation and writing for extraction representations.

use std::io::Write;

use thiserror::Error;
#[cfg(feature = "decode")]
use unity_asset_core::SourceLocator;
use unity_asset_core::{AssetLoadBudget, BudgetError, ObjectAddress, UnityValue};
use unity_asset_yaml::UnityYamlSerializer;

#[cfg(feature = "decode")]
use unity_asset_decode::{
    audio::{AudioClipLayout, AudioExporter, AudioSourceError, PreparedAudioSource},
    media::{BudgetedMediaBytes, EmbeddedMediaError, MediaPayloadRef},
    sprite::{PreparedSpritePng, SpriteLayout, SpritePreparationError},
    texture::{PreparedTexturePng, Texture2DLayout, TexturePreparationError},
};

use super::super::source_budget_error;
#[cfg(feature = "decode")]
use super::contract::PlannedStreamSource;
use super::contract::{PlannedContent, RepresentationContract};
#[cfg(feature = "decode")]
use super::payload::{WorkspacePayloadError, copy_workspace_range};
use crate::workspace::{
    WorkspaceError, WorkspaceLookup, WorkspaceObject, WorkspaceObjectValue, WorkspaceView,
};

/// Opaque source or codec state proven before a representation can enter staging.
pub(in crate::extraction) struct PreparedRepresentation {
    state: PreparedState,
}

enum PreparedState {
    Source {
        object: WorkspaceObject,
        preferred: PreparedSource,
        raw_fallback: bool,
    },
    #[cfg(feature = "decode")]
    Media {
        preferred: PreparedMedia,
        raw_fallback: Option<WorkspaceObject>,
    },
}

enum PreparedSource {
    RawBinary,
    Yaml,
    TextAsset,
    #[cfg(not(feature = "decode"))]
    DecodedUnavailable,
}

#[cfg(feature = "decode")]
enum PreparedMedia {
    Audio(PreparedAudioSource),
    Texture(PreparedTexturePng),
    Sprite(PreparedSpritePng),
}

impl PreparedRepresentation {
    pub(in crate::extraction) fn prepare(
        view: &dyn WorkspaceView,
        address: &ObjectAddress,
        contract: &RepresentationContract,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, RepresentationPreparationError> {
        let object = read_object(view, address, budget)?;
        let raw_fallback = contract.fallback().is_some();
        match contract.preferred_content() {
            PlannedContent::RawBinary => Ok(Self::source(
                object,
                PreparedSource::RawBinary,
                raw_fallback,
            )),
            PlannedContent::Yaml => Ok(Self::source(object, PreparedSource::Yaml, raw_fallback)),
            PlannedContent::TextAsset => Ok(Self::source(
                object,
                PreparedSource::TextAsset,
                raw_fallback,
            )),
            #[cfg(feature = "decode")]
            PlannedContent::Audio { stream, descriptor } => {
                let WorkspaceObjectValue::Binary(binary) = object.value() else {
                    return Err(RepresentationPreparationError::InvalidContent);
                };
                let layout = AudioClipLayout::inspect(binary)
                    .map_err(|_| RepresentationPreparationError::InvalidContent)?;
                let source = media_source_bytes(view, layout.payload(), stream.as_ref(), budget)?;
                let media = AudioExporter::prepare_layout(layout, source, budget)
                    .map_err(map_audio_preparation_error)?;
                validate_descriptor(descriptor, media.descriptor())?;
                Ok(Self::media(
                    PreparedMedia::Audio(media),
                    object,
                    raw_fallback,
                ))
            }
            #[cfg(feature = "decode")]
            PlannedContent::TexturePng { stream, descriptor } => {
                let WorkspaceObjectValue::Binary(binary) = object.value() else {
                    return Err(RepresentationPreparationError::InvalidContent);
                };
                let layout = Texture2DLayout::inspect(binary)
                    .map_err(|_| RepresentationPreparationError::InvalidContent)?;
                let source = media_source_bytes(view, layout.payload(), stream.as_ref(), budget)?;
                let media = PreparedTexturePng::prepare(layout, source, budget)
                    .map_err(map_texture_preparation_error)?;
                validate_descriptor(descriptor, media.descriptor())?;
                Ok(Self::media(
                    PreparedMedia::Texture(media),
                    object,
                    raw_fallback,
                ))
            }
            #[cfg(feature = "decode")]
            PlannedContent::SpritePng {
                texture,
                texture_stream,
                descriptor,
            } => {
                let WorkspaceObjectValue::Binary(sprite) = object.value() else {
                    return Err(RepresentationPreparationError::InvalidContent);
                };
                let sprite_layout = SpriteLayout::inspect(sprite)
                    .map_err(|_| RepresentationPreparationError::InvalidContent)?;
                let texture = read_object(view, texture, budget)?;
                let WorkspaceObjectValue::Binary(texture) = texture.value() else {
                    return Err(RepresentationPreparationError::InvalidContent);
                };
                let texture_layout = Texture2DLayout::inspect(texture)
                    .map_err(|_| RepresentationPreparationError::InvalidContent)?;
                let source = media_source_bytes(
                    view,
                    texture_layout.payload(),
                    texture_stream.as_ref(),
                    budget,
                )?;
                let media =
                    PreparedSpritePng::prepare(sprite_layout, texture_layout, source, budget)
                        .map_err(map_sprite_preparation_error)?;
                validate_descriptor(descriptor, media.descriptor())?;
                Ok(Self::media(
                    PreparedMedia::Sprite(media),
                    object,
                    raw_fallback,
                ))
            }
            #[cfg(not(feature = "decode"))]
            PlannedContent::Audio { .. }
            | PlannedContent::TexturePng { .. }
            | PlannedContent::SpritePng { .. } => {
                if raw_fallback {
                    Ok(Self::source(
                        object,
                        PreparedSource::DecodedUnavailable,
                        true,
                    ))
                } else {
                    Err(RepresentationPreparationError::InvalidContent)
                }
            }
        }
    }

    fn source(object: WorkspaceObject, preferred: PreparedSource, raw_fallback: bool) -> Self {
        Self {
            state: PreparedState::Source {
                object,
                preferred,
                raw_fallback,
            },
        }
    }

    #[cfg(feature = "decode")]
    fn media(preferred: PreparedMedia, object: WorkspaceObject, raw_fallback: bool) -> Self {
        Self {
            state: PreparedState::Media {
                preferred,
                raw_fallback: raw_fallback.then_some(object),
            },
        }
    }

    pub(in crate::extraction) fn write_preferred(
        &self,
        writer: &mut dyn Write,
        budget: Option<&mut AssetLoadBudget>,
    ) -> Result<(), RepresentationWriteError> {
        match &self.state {
            PreparedState::Source {
                object,
                preferred: PreparedSource::RawBinary,
                ..
            } => write_raw_binary(writer, object),
            PreparedState::Source {
                object,
                preferred: PreparedSource::Yaml,
                ..
            } => {
                let budget = budget.ok_or(RepresentationWriteError::InvalidContent)?;
                UnityYamlSerializer::new()
                    .serialize_to_writer_with_budget(
                        writer,
                        std::iter::once(object.class()),
                        budget,
                    )
                    .map_err(|error| match source_budget_error(&error) {
                        Some(error) => RepresentationWriteError::Budget(error.clone()),
                        None => RepresentationWriteError::Output,
                    })
            }
            PreparedState::Source {
                object,
                preferred: PreparedSource::TextAsset,
                ..
            } => write_text_asset(writer, object),
            #[cfg(not(feature = "decode"))]
            PreparedState::Source {
                preferred: PreparedSource::DecodedUnavailable,
                ..
            } => Err(RepresentationWriteError::CapabilityUnavailable {
                capability: "media decode",
            }),
            #[cfg(feature = "decode")]
            PreparedState::Media { preferred, .. } => write_media(writer, preferred),
        }
    }

    pub(in crate::extraction) fn write_fallback(
        &self,
        writer: &mut dyn Write,
    ) -> Result<(), RepresentationWriteError> {
        let object = match &self.state {
            PreparedState::Source {
                object,
                raw_fallback: true,
                ..
            } => object,
            #[cfg(feature = "decode")]
            PreparedState::Media {
                raw_fallback: Some(object),
                ..
            } => object,
            PreparedState::Source {
                raw_fallback: false,
                ..
            } => return Err(RepresentationWriteError::InvalidContent),
            #[cfg(feature = "decode")]
            PreparedState::Media {
                raw_fallback: None, ..
            } => return Err(RepresentationWriteError::InvalidContent),
        };
        write_raw_binary(writer, object)
    }
}

fn write_raw_binary(
    writer: &mut dyn Write,
    object: &WorkspaceObject,
) -> Result<(), RepresentationWriteError> {
    let WorkspaceObjectValue::Binary(object) = object.value() else {
        return Err(RepresentationWriteError::InvalidContent);
    };
    writer
        .write_all(object.raw_data())
        .map_err(|_| RepresentationWriteError::Output)
}

#[derive(Debug, Error)]
pub(in crate::extraction) enum RepresentationPreparationError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[cfg(feature = "decode")]
    #[error("failed to allocate {resource} ({requested} bytes)")]
    Allocation {
        resource: &'static str,
        requested: usize,
    },
    #[cfg(feature = "decode")]
    #[error("streamed media source changed: {0:?}")]
    SourceChanged(SourceLocator),
    #[cfg(feature = "decode")]
    #[error("prepared media descriptor changed")]
    DescriptorChanged,
    #[error("planned representation no longer matches its source object")]
    InvalidContent,
}

#[derive(Debug, Error)]
pub(in crate::extraction) enum RepresentationWriteError {
    #[error("prepared representation no longer matches the requested content")]
    InvalidContent,
    #[cfg(not(feature = "decode"))]
    #[error("prepared representation requires unavailable capability {capability}")]
    CapabilityUnavailable { capability: &'static str },
    #[error("representation output failed")]
    Output,
    #[error(transparent)]
    Budget(BudgetError),
    #[cfg(feature = "decode")]
    #[error("failed to allocate {resource} ({requested} bytes)")]
    Allocation {
        resource: &'static str,
        requested: usize,
    },
}

fn read_object(
    view: &dyn WorkspaceView,
    address: &ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceObject, RepresentationPreparationError> {
    let handle = match view.resolve_object(address, budget)? {
        WorkspaceLookup::Resolved(handle) => handle,
        WorkspaceLookup::Unloaded
        | WorkspaceLookup::Missing
        | WorkspaceLookup::Ambiguous { .. }
        | WorkspaceLookup::Invalid { .. } => {
            return Err(WorkspaceError::MissingObject(Box::new(address.clone())).into());
        }
    };
    view.read_object(&handle, budget).map_err(Into::into)
}

#[cfg(feature = "decode")]
fn media_source_bytes(
    view: &dyn WorkspaceView,
    payload: MediaPayloadRef<'_>,
    stream: Option<&PlannedStreamSource>,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedMediaBytes, RepresentationPreparationError> {
    match (payload.embedded(), stream) {
        (None, Some(source)) => read_streamed_payload(view, source, budget),
        (Some(embedded), None) => embedded
            .materialize("embedded media payload", budget)
            .map_err(map_embedded_media_error),
        _ => Err(RepresentationPreparationError::InvalidContent),
    }
}

#[cfg(feature = "decode")]
fn read_streamed_payload(
    view: &dyn WorkspaceView,
    source: &PlannedStreamSource,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedMediaBytes, RepresentationPreparationError> {
    let range = source.open(view, budget)?;
    copy_workspace_range(&range, "extraction streamed resource", budget)
        .map_err(|error| map_workspace_payload_error(error, source.request().owner()))
}

#[cfg(feature = "decode")]
fn map_embedded_media_error(error: EmbeddedMediaError) -> RepresentationPreparationError {
    match error {
        EmbeddedMediaError::Budget(error) => RepresentationPreparationError::Budget(error),
        EmbeddedMediaError::Allocation {
            resource,
            requested,
            ..
        } => RepresentationPreparationError::Allocation {
            resource,
            requested,
        },
        EmbeddedMediaError::EvidenceChanged => RepresentationPreparationError::InvalidContent,
    }
}

#[cfg(feature = "decode")]
fn map_workspace_payload_error(
    error: WorkspacePayloadError,
    owner: &SourceLocator,
) -> RepresentationPreparationError {
    match error {
        WorkspacePayloadError::Budget(error) => RepresentationPreparationError::Budget(error),
        WorkspacePayloadError::LengthOverflow { resource } => {
            RepresentationPreparationError::Allocation {
                resource,
                requested: usize::MAX,
            }
        }
        WorkspacePayloadError::Allocation {
            resource,
            requested,
            ..
        } => RepresentationPreparationError::Allocation {
            resource,
            requested,
        },
        WorkspacePayloadError::Read { .. } => {
            RepresentationPreparationError::SourceChanged(owner.clone())
        }
    }
}

#[cfg(feature = "decode")]
fn validate_descriptor(
    expected: &unity_asset_decode::descriptor::MediaDescriptor,
    actual: &unity_asset_decode::descriptor::MediaDescriptor,
) -> Result<(), RepresentationPreparationError> {
    if expected == actual {
        Ok(())
    } else {
        Err(RepresentationPreparationError::DescriptorChanged)
    }
}

#[cfg(feature = "decode")]
fn map_audio_preparation_error(error: AudioSourceError) -> RepresentationPreparationError {
    match error {
        AudioSourceError::Budget(error) => RepresentationPreparationError::Budget(error),
        AudioSourceError::Allocation {
            resource,
            requested,
            ..
        } => RepresentationPreparationError::Allocation {
            resource,
            requested,
        },
        AudioSourceError::InvalidData(_)
        | AudioSourceError::UnsupportedFormat(_)
        | AudioSourceError::UnsupportedContainer { .. }
        | AudioSourceError::Descriptor(_)
        | AudioSourceError::Output(_) => RepresentationPreparationError::InvalidContent,
    }
}

#[cfg(feature = "decode")]
fn map_texture_preparation_error(error: TexturePreparationError) -> RepresentationPreparationError {
    match error {
        TexturePreparationError::Budget(error) => RepresentationPreparationError::Budget(error),
        TexturePreparationError::Allocation {
            resource,
            requested,
            ..
        } => RepresentationPreparationError::Allocation {
            resource,
            requested,
        },
        TexturePreparationError::SourceLengthMismatch { .. }
        | TexturePreparationError::UnsupportedFormat(_)
        | TexturePreparationError::LengthOverflow(_)
        | TexturePreparationError::Descriptor(_)
        | TexturePreparationError::Decode(_)
        | TexturePreparationError::Output(_) => RepresentationPreparationError::InvalidContent,
    }
}

#[cfg(feature = "decode")]
fn map_sprite_preparation_error(error: SpritePreparationError) -> RepresentationPreparationError {
    match error {
        SpritePreparationError::Budget(error) => RepresentationPreparationError::Budget(error),
        SpritePreparationError::Allocation {
            resource,
            requested,
            ..
        } => RepresentationPreparationError::Allocation {
            resource,
            requested,
        },
        SpritePreparationError::Texture(error) => map_texture_preparation_error(error),
        SpritePreparationError::InvalidSpriteRect
        | SpritePreparationError::LengthOverflow(_)
        | SpritePreparationError::Descriptor(_)
        | SpritePreparationError::Output(_) => RepresentationPreparationError::InvalidContent,
    }
}

fn write_text_asset(
    writer: &mut dyn Write,
    object: &WorkspaceObject,
) -> Result<(), RepresentationWriteError> {
    let WorkspaceObjectValue::Binary(object) = object.value() else {
        return Err(RepresentationWriteError::InvalidContent);
    };
    for key in ["m_Script", "m_Text", "m_Bytes", "m_Data"] {
        let Some(value) = object.get(key) else {
            continue;
        };
        return match value {
            UnityValue::String(value) => writer
                .write_all(value.as_bytes())
                .map_err(|_| RepresentationWriteError::Output),
            UnityValue::Bytes(value) => writer
                .write_all(value)
                .map_err(|_| RepresentationWriteError::Output),
            UnityValue::Array(values) => write_byte_array(writer, values),
            _ => Err(RepresentationWriteError::InvalidContent),
        };
    }
    Err(RepresentationWriteError::InvalidContent)
}

fn write_byte_array(
    writer: &mut dyn Write,
    values: &[UnityValue],
) -> Result<(), RepresentationWriteError> {
    let mut buffer = [0_u8; 8192];
    for chunk in values.chunks(buffer.len()) {
        for (output, value) in buffer.iter_mut().zip(chunk) {
            *output = value
                .as_u64()
                .and_then(|value| u8::try_from(value).ok())
                .ok_or(RepresentationWriteError::InvalidContent)?;
        }
        writer
            .write_all(&buffer[..chunk.len()])
            .map_err(|_| RepresentationWriteError::Output)?;
    }
    Ok(())
}

#[cfg(feature = "decode")]
fn write_media(
    writer: &mut dyn Write,
    media: &PreparedMedia,
) -> Result<(), RepresentationWriteError> {
    match media {
        PreparedMedia::Audio(prepared) => write_audio(writer, prepared),
        PreparedMedia::Texture(prepared) => write_texture(writer, prepared),
        PreparedMedia::Sprite(prepared) => write_sprite(writer, prepared),
    }
}

#[cfg(feature = "decode")]
fn write_audio(
    writer: &mut dyn Write,
    prepared: &PreparedAudioSource,
) -> Result<(), RepresentationWriteError> {
    prepared.write_to(writer).map_err(|error| match error {
        AudioSourceError::Output(_) => RepresentationWriteError::Output,
        AudioSourceError::Budget(error) => RepresentationWriteError::Budget(error),
        AudioSourceError::Allocation {
            resource,
            requested,
            ..
        } => RepresentationWriteError::Allocation {
            resource,
            requested,
        },
        AudioSourceError::InvalidData(_)
        | AudioSourceError::UnsupportedFormat(_)
        | AudioSourceError::UnsupportedContainer { .. }
        | AudioSourceError::Descriptor(_) => RepresentationWriteError::InvalidContent,
    })
}

#[cfg(feature = "decode")]
fn write_texture(
    writer: &mut dyn Write,
    prepared: &PreparedTexturePng,
) -> Result<(), RepresentationWriteError> {
    prepared.write_to(writer).map_err(|error| match error {
        TexturePreparationError::Output(_) => RepresentationWriteError::Output,
        TexturePreparationError::Budget(error) => RepresentationWriteError::Budget(error),
        TexturePreparationError::Allocation {
            resource,
            requested,
            ..
        } => RepresentationWriteError::Allocation {
            resource,
            requested,
        },
        TexturePreparationError::SourceLengthMismatch { .. }
        | TexturePreparationError::UnsupportedFormat(_)
        | TexturePreparationError::LengthOverflow(_)
        | TexturePreparationError::Descriptor(_)
        | TexturePreparationError::Decode(_) => RepresentationWriteError::InvalidContent,
    })
}

#[cfg(feature = "decode")]
fn write_sprite(
    writer: &mut dyn Write,
    prepared: &PreparedSpritePng,
) -> Result<(), RepresentationWriteError> {
    prepared.write_to(writer).map_err(|error| match error {
        SpritePreparationError::Output(_) => RepresentationWriteError::Output,
        SpritePreparationError::Budget(error)
        | SpritePreparationError::Texture(TexturePreparationError::Budget(error)) => {
            RepresentationWriteError::Budget(error)
        }
        SpritePreparationError::Allocation {
            resource,
            requested,
            ..
        }
        | SpritePreparationError::Texture(TexturePreparationError::Allocation {
            resource,
            requested,
            ..
        }) => RepresentationWriteError::Allocation {
            resource,
            requested,
        },
        SpritePreparationError::InvalidSpriteRect
        | SpritePreparationError::LengthOverflow(_)
        | SpritePreparationError::Descriptor(_)
        | SpritePreparationError::Texture(_) => RepresentationWriteError::InvalidContent,
    })
}
