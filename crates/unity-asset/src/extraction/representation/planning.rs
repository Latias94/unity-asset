//! Planning-time selection for extraction representations.

use unity_asset_binary::asset::class_ids;
#[cfg(feature = "decode")]
use unity_asset_core::RevisionedObjectHandle;
use unity_asset_core::{AssetLoadBudget, ObjectAddress, SourceFingerprint};
#[cfg(feature = "decode")]
use unity_asset_decode::{
    audio::{AudioClipLayout, AudioExporter, AudioSourceError},
    media::{BudgetedMediaBytes, EmbeddedMediaError, EmbeddedMediaRef, MediaInspectionError},
    sprite::{PreparedSpritePng, SpriteLayout, SpritePreparationError},
    texture::{PreparedTexturePng, Texture2DLayout, TexturePreparationError},
};

#[cfg(feature = "decode")]
use super::super::contract::ExtractionAllocationUnit;
use super::super::contract::{
    ExtractionDiagnostic, ExtractionDiagnosticCode, ExtractionPath, ExtractionRepresentationPolicy,
    ExtractionSourceExpectation,
};
use super::super::planning_contract::{
    ExtractionPlanError, clone_object_address, clone_source_locator, push_budgeted,
};
#[cfg(feature = "decode")]
use super::super::planning_contract::{
    budgeted_vec, clone_string, resolve_required_handle, source_for_id,
};
use super::contract::{
    PlannedContent, PlannedFallback, RepresentationContract, RepresentationContractParts,
};
#[cfg(feature = "decode")]
use super::payload::{WorkspacePayloadError, copy_workspace_range};
use super::reservation::{ExtractionReservationError, raw_binary_working_set, yaml_working_set};
#[cfg(feature = "decode")]
use super::reservation::{audio_working_set, sprite_working_set_with_texture, texture_working_set};
use crate::reference::ReferenceGraph;
#[cfg(feature = "decode")]
use crate::reference::{RawReferenceTarget, ReferenceResolution};
#[cfg(feature = "decode")]
use crate::workspace::WorkspaceError;
#[cfg(feature = "decode")]
use crate::workspace::{
    ResolvedStreamedResource, StreamedResourceRequest, StreamedResourceRequestError,
    StreamedResourceResolution, StreamedResourceResolver,
};
use crate::workspace::{WorkspaceObject, WorkspaceObjectValue, WorkspaceSource, WorkspaceView};

/// Workspace-bound selector for one inert extraction representation.
pub(in crate::extraction) struct RepresentationPlanner<'view, 'source> {
    policy: ExtractionRepresentationPolicy,
    #[cfg(feature = "decode")]
    view: &'view dyn WorkspaceView,
    #[cfg(feature = "decode")]
    references: Option<&'view ReferenceGraph>,
    #[cfg(feature = "decode")]
    sources: &'source [WorkspaceSource],
    #[cfg(feature = "decode")]
    stream_resolver: Option<StreamedResourceResolver<'view, 'source>>,
    #[cfg(not(feature = "decode"))]
    marker: std::marker::PhantomData<(&'view dyn WorkspaceView, &'source [WorkspaceSource])>,
}

impl<'view, 'source> RepresentationPlanner<'view, 'source> {
    pub(in crate::extraction) fn new(
        view: &'view dyn WorkspaceView,
        references: Option<&'view ReferenceGraph>,
        sources: &'source [WorkspaceSource],
        policy: ExtractionRepresentationPolicy,
    ) -> Self {
        #[cfg(feature = "decode")]
        {
            Self {
                policy,
                view,
                references,
                sources,
                stream_resolver: None,
            }
        }
        #[cfg(not(feature = "decode"))]
        {
            let _ = (view, references, sources);
            Self {
                policy,
                marker: std::marker::PhantomData,
            }
        }
    }

    pub(in crate::extraction) fn select(
        &mut self,
        address: &ObjectAddress,
        object: &WorkspaceObject,
        owner: &WorkspaceSource,
        budget: &mut AssetLoadBudget,
    ) -> Result<RepresentationChoice, ExtractionPlanError> {
        let raw = raw_content(object, budget)?;
        if self.policy == ExtractionRepresentationPolicy::RawOnly
            || matches!(object.value(), WorkspaceObjectValue::Yaml(_))
        {
            return Ok(raw);
        }

        let WorkspaceObjectValue::Binary(binary) = object.value() else {
            return Ok(raw);
        };
        if binary.class_id() == class_ids::TEXT_ASSET {
            return Ok(RepresentationChoice::decoded(
                PlannedContent::TextAsset,
                raw_binary_working_set(object).map_err(map_reservation_error)?,
                self.policy.raw_fallback(),
            ));
        }

        #[cfg(not(feature = "decode"))]
        {
            let _ = owner;
            unavailable_choice_with(
                address,
                self.policy,
                raw,
                ExtractionDiagnosticCode::FeatureUnavailable,
                budget,
            )
        }

        #[cfg(feature = "decode")]
        {
            match binary.class_id() {
                class_ids::AUDIO_CLIP => self.select_audio(address, object, owner, raw, budget),
                class_ids::TEXTURE_2D => self.select_texture(address, object, owner, raw, budget),
                class_ids::SPRITE => self.select_sprite(address, object, owner, raw, budget),
                _ => unavailable_choice_with(
                    address,
                    self.policy,
                    raw,
                    ExtractionDiagnosticCode::UnsupportedClass,
                    budget,
                ),
            }
        }
    }

    #[cfg(feature = "decode")]
    fn select_audio(
        &mut self,
        address: &ObjectAddress,
        object: &WorkspaceObject,
        owner: &WorkspaceSource,
        raw: RepresentationChoice,
        budget: &mut AssetLoadBudget,
    ) -> Result<RepresentationChoice, ExtractionPlanError> {
        let WorkspaceObjectValue::Binary(binary) = object.value() else {
            unreachable!("audio planning is only dispatched for binary objects");
        };
        let layout =
            match classify_media_inspection(address, AudioClipLayout::inspect(binary), budget)? {
                MediaInspectionOutcome::Prepared(layout) => layout,
                MediaInspectionOutcome::Unavailable(reason) => {
                    return unavailable_choice_with(address, self.policy, raw, reason, budget);
                }
            };
        if !AudioExporter::supports_standard_source(layout.compression_format()) {
            return unavailable_choice_with(
                address,
                self.policy,
                raw,
                ExtractionDiagnosticCode::UnsupportedMediaEncoding,
                budget,
            );
        }
        let (stream, stream_expectation, bytes) = if let Some(stream) = layout.payload().stream() {
            match self.resolve_stream(owner, stream.path(), stream.offset(), stream.size(), budget)
            {
                Ok(resolved) => (
                    Some(resolved.request),
                    Some(resolved.expectation),
                    resolved.bytes,
                ),
                Err(error) => {
                    let Some(code) = decoded_resource_failure(&error) else {
                        return Err(error);
                    };
                    return unavailable_choice_with(address, self.policy, raw, code, budget);
                }
            }
        } else {
            (
                None,
                None,
                materialize_embedded(
                    layout.payload().embedded(),
                    "planned embedded audio",
                    budget,
                )?,
            )
        };
        let prepared = match AudioExporter::prepare_layout(layout, bytes, budget) {
            Ok(prepared) => prepared,
            Err(AudioSourceError::Budget(error)) => return Err(error.into()),
            Err(AudioSourceError::Allocation {
                resource,
                requested,
                ..
            }) => {
                return Err(ExtractionPlanError::MediaAllocation {
                    resource,
                    requested,
                });
            }
            Err(error @ AudioSourceError::UnsupportedFormat(_))
            | Err(error @ AudioSourceError::UnsupportedContainer { .. }) => {
                return unsupported_audio_choice(address, self.policy, raw, error, budget);
            }
            Err(
                AudioSourceError::InvalidData(_)
                | AudioSourceError::Descriptor(_)
                | AudioSourceError::Output(_),
            ) => return media_preparation_error(address, budget),
        };
        let descriptor = prepared.descriptor().clone();
        let mut expectations = budgeted_vec(
            usize::from(stream_expectation.is_some()),
            "audio source expectations",
            budget,
        )?;
        if let Some(expectation) = stream_expectation {
            expectations.push(expectation);
        }
        let working_set = audio_working_set(
            self.view,
            object,
            &descriptor,
            stream.as_ref(),
            self.stream_resolver.as_ref(),
            budget,
        )
        .map_err(map_reservation_error)?;
        Ok(RepresentationChoice::decoded_with_sources(
            PlannedContent::Audio { stream, descriptor },
            working_set,
            self.policy.raw_fallback(),
            expectations,
        ))
    }

    #[cfg(feature = "decode")]
    fn select_texture(
        &mut self,
        address: &ObjectAddress,
        object: &WorkspaceObject,
        owner: &WorkspaceSource,
        raw: RepresentationChoice,
        budget: &mut AssetLoadBudget,
    ) -> Result<RepresentationChoice, ExtractionPlanError> {
        let WorkspaceObjectValue::Binary(binary) = object.value() else {
            unreachable!("texture planning is only dispatched for binary objects");
        };
        let layout =
            match classify_media_inspection(address, Texture2DLayout::inspect(binary), budget)? {
                MediaInspectionOutcome::Prepared(layout) => layout,
                MediaInspectionOutcome::Unavailable(reason) => {
                    return unavailable_choice_with(address, self.policy, raw, reason, budget);
                }
            };
        let (stream, stream_expectation, bytes) = if let Some(stream) = layout.payload().stream() {
            match self.resolve_stream(owner, stream.path(), stream.offset(), stream.size(), budget)
            {
                Ok(resolved) => (
                    Some(resolved.request),
                    Some(resolved.expectation),
                    resolved.bytes,
                ),
                Err(error) => {
                    let Some(code) = decoded_resource_failure(&error) else {
                        return Err(error);
                    };
                    return unavailable_choice_with(address, self.policy, raw, code, budget);
                }
            }
        } else {
            (
                None,
                None,
                materialize_embedded(
                    layout.payload().embedded(),
                    "planned embedded texture",
                    budget,
                )?,
            )
        };
        let prepared = match PreparedTexturePng::prepare(layout, bytes, budget) {
            Ok(prepared) => prepared,
            Err(TexturePreparationError::Budget(error)) => return Err(error.into()),
            Err(TexturePreparationError::Allocation {
                resource,
                requested,
                ..
            }) => {
                return Err(ExtractionPlanError::MediaAllocation {
                    resource,
                    requested,
                });
            }
            Err(TexturePreparationError::UnsupportedFormat(_)) => {
                return unavailable_choice_with(
                    address,
                    self.policy,
                    raw,
                    ExtractionDiagnosticCode::UnsupportedMediaEncoding,
                    budget,
                );
            }
            Err(
                TexturePreparationError::SourceLengthMismatch { .. }
                | TexturePreparationError::LengthOverflow(_)
                | TexturePreparationError::Descriptor(_)
                | TexturePreparationError::Decode(_)
                | TexturePreparationError::Output(_),
            ) => return media_preparation_error(address, budget),
        };
        let descriptor = prepared.descriptor().clone();
        let mut expectations = budgeted_vec(
            usize::from(stream_expectation.is_some()),
            "texture source expectations",
            budget,
        )?;
        if let Some(expectation) = stream_expectation {
            expectations.push(expectation);
        }
        let working_set = texture_working_set(
            self.view,
            object,
            &descriptor,
            stream.as_ref(),
            self.stream_resolver.as_ref(),
            budget,
        )
        .map_err(map_reservation_error)?;
        Ok(RepresentationChoice::decoded_with_sources(
            PlannedContent::TexturePng { stream, descriptor },
            working_set,
            self.policy.raw_fallback(),
            expectations,
        ))
    }

    #[cfg(feature = "decode")]
    fn select_sprite(
        &mut self,
        address: &ObjectAddress,
        object: &WorkspaceObject,
        owner: &WorkspaceSource,
        raw: RepresentationChoice,
        budget: &mut AssetLoadBudget,
    ) -> Result<RepresentationChoice, ExtractionPlanError> {
        let WorkspaceObjectValue::Binary(binary) = object.value() else {
            unreachable!("sprite planning is only dispatched for binary objects");
        };
        let sprite_layout =
            match classify_media_inspection(address, SpriteLayout::inspect(binary), budget)? {
                MediaInspectionOutcome::Prepared(layout) => layout,
                MediaInspectionOutcome::Unavailable(reason) => {
                    return unavailable_choice_with(address, self.policy, raw, reason, budget);
                }
            };
        let texture_reference = sprite_layout.texture();
        let file_id = texture_reference.file_id();
        let path_id = texture_reference.path_id();
        let texture_address = if file_id == 0 {
            ObjectAddress::binary_at(
                clone_source_locator(owner.locator(), "sprite texture source locator", budget)?,
                path_id,
            )?
        } else {
            let Some(references) = self.references else {
                return unavailable_choice_with(
                    address,
                    self.policy,
                    raw,
                    ExtractionDiagnosticCode::UnresolvedSpritePPtr,
                    budget,
                );
            };
            let owner_handle = resolve_required_handle(self.view, address, budget)?;
            let Some(texture) =
                resolve_reference_address(references, &owner_handle, file_id, path_id, budget)?
            else {
                return unavailable_choice_with(
                    address,
                    self.policy,
                    raw,
                    ExtractionDiagnosticCode::UnresolvedSpritePPtr,
                    budget,
                );
            };
            texture
        };
        let texture_handle = match resolve_required_handle(self.view, &texture_address, budget) {
            Ok(handle) => handle,
            Err(error) => {
                let Some(code) = sprite_reference_failure(&error) else {
                    return Err(error);
                };
                return unavailable_choice_with(address, self.policy, raw, code, budget);
            }
        };
        let texture_object = self.view.read_object(&texture_handle, budget)?;
        let WorkspaceObjectValue::Binary(texture_binary) = texture_object.value() else {
            return unavailable_choice_with(
                address,
                self.policy,
                raw,
                ExtractionDiagnosticCode::UnresolvedSpritePPtr,
                budget,
            );
        };
        if texture_binary.class_id() != class_ids::TEXTURE_2D {
            return unavailable_choice_with(
                address,
                self.policy,
                raw,
                ExtractionDiagnosticCode::UnresolvedSpritePPtr,
                budget,
            );
        }
        let texture_owner = match source_for_id(texture_handle.object().source(), self.sources) {
            Ok(owner) => owner,
            Err(error) => {
                let Some(code) = decoded_resource_failure(&error) else {
                    return Err(error);
                };
                return unavailable_choice_with(address, self.policy, raw, code, budget);
            }
        };
        let texture_layout = match classify_media_inspection(
            address,
            Texture2DLayout::inspect(texture_binary),
            budget,
        )? {
            MediaInspectionOutcome::Prepared(layout) => layout,
            MediaInspectionOutcome::Unavailable(reason) => {
                return unavailable_choice_with(address, self.policy, raw, reason, budget);
            }
        };
        let (texture_stream, stream_expectation, texture_bytes) =
            if let Some(stream) = texture_layout.payload().stream() {
                match self.resolve_stream(
                    texture_owner,
                    stream.path(),
                    stream.offset(),
                    stream.size(),
                    budget,
                ) {
                    Ok(resolved) => (
                        Some(resolved.request),
                        Some(resolved.expectation),
                        resolved.bytes,
                    ),
                    Err(error) => {
                        let Some(code) = decoded_resource_failure(&error) else {
                            return Err(error);
                        };
                        return unavailable_choice_with(address, self.policy, raw, code, budget);
                    }
                }
            } else {
                (
                    None,
                    None,
                    materialize_embedded(
                        texture_layout.payload().embedded(),
                        "planned embedded sprite texture",
                        budget,
                    )?,
                )
            };
        let prepared = match PreparedSpritePng::prepare(
            sprite_layout,
            texture_layout,
            texture_bytes,
            budget,
        ) {
            Ok(prepared) => prepared,
            Err(SpritePreparationError::Budget(error))
            | Err(SpritePreparationError::Texture(TexturePreparationError::Budget(error))) => {
                return Err(error.into());
            }
            Err(SpritePreparationError::Texture(TexturePreparationError::Allocation {
                resource,
                requested,
                ..
            }))
            | Err(SpritePreparationError::Allocation {
                resource,
                requested,
                ..
            }) => {
                return Err(ExtractionPlanError::MediaAllocation {
                    resource,
                    requested,
                });
            }
            Err(SpritePreparationError::Texture(TexturePreparationError::UnsupportedFormat(_))) => {
                return unavailable_choice_with(
                    address,
                    self.policy,
                    raw,
                    ExtractionDiagnosticCode::UnsupportedMediaEncoding,
                    budget,
                );
            }
            Err(
                SpritePreparationError::InvalidSpriteRect
                | SpritePreparationError::LengthOverflow(_)
                | SpritePreparationError::Descriptor(_)
                | SpritePreparationError::Output(_)
                | SpritePreparationError::Texture(_),
            ) => return media_preparation_error(address, budget),
        };
        let descriptor = prepared.descriptor().clone();
        let mut expectations = budgeted_vec(
            1 + usize::from(stream_expectation.is_some()),
            "sprite source expectations",
            budget,
        )?;
        expectations.push(source_expectation(texture_owner, budget)?);
        if let Some(expectation) = stream_expectation {
            expectations.push(expectation);
        }
        let working_set = sprite_working_set_with_texture(
            self.view,
            object,
            &texture_object,
            &descriptor,
            texture_stream.as_ref(),
            self.stream_resolver.as_ref(),
            budget,
        )
        .map_err(map_reservation_error)?;
        Ok(RepresentationChoice::decoded_with_sources(
            PlannedContent::SpritePng {
                texture: texture_address,
                texture_stream,
                descriptor,
            },
            working_set,
            self.policy.raw_fallback(),
            expectations,
        ))
    }

    #[cfg(feature = "decode")]
    fn resolve_stream(
        &mut self,
        owner: &WorkspaceSource,
        stream_path: &str,
        offset: u64,
        size: u64,
        budget: &mut AssetLoadBudget,
    ) -> Result<ResolvedExtractionStream, ExtractionPlanError> {
        if self.stream_resolver.is_none() {
            self.stream_resolver = Some(StreamedResourceResolver::new(
                self.view,
                self.sources,
                budget,
            )?);
        }
        resolve_extraction_stream(
            self.view,
            owner,
            self.stream_resolver
                .as_ref()
                .expect("stream resolver was initialized above"),
            stream_path,
            offset,
            size,
            budget,
        )
    }
}

#[derive(Debug)]
pub(in crate::extraction) struct RepresentationChoice {
    preferred_content: PlannedContent,
    raw_fallback: bool,
    working_set_bytes: u64,
    diagnostics: Vec<ExtractionDiagnostic>,
    source_expectations: Vec<ExtractionSourceExpectation>,
}

impl RepresentationChoice {
    fn decoded(content: PlannedContent, working_set_bytes: u64, raw_fallback: bool) -> Self {
        Self::decoded_with_sources(content, working_set_bytes, raw_fallback, Vec::new())
    }

    fn decoded_with_sources(
        content: PlannedContent,
        working_set_bytes: u64,
        raw_fallback: bool,
        source_expectations: Vec<ExtractionSourceExpectation>,
    ) -> Self {
        Self {
            preferred_content: content,
            raw_fallback,
            working_set_bytes: working_set_bytes.max(1),
            diagnostics: Vec::new(),
            source_expectations,
        }
    }

    pub(in crate::extraction) fn finalize<F>(
        mut self,
        ordinal: u32,
        address: &ObjectAddress,
        expectations: &mut Vec<ExtractionSourceExpectation>,
        owner: &WorkspaceSource,
        budget: &mut AssetLoadBudget,
        mut allocate_path: F,
    ) -> Result<RepresentationContract, ExtractionPlanError>
    where
        F: FnMut(
            &'static str,
            bool,
            &mut AssetLoadBudget,
        ) -> Result<ExtractionPath, ExtractionPlanError>,
    {
        let owner = source_expectation(owner, budget)?;
        if let Some(first) = conflicting_fingerprint(expectations, &owner) {
            let (locator, second) = owner.into_parts();
            return Err(ExtractionPlanError::SourceFingerprintConflict {
                locator,
                first,
                second,
            });
        }
        for index in 0..self.source_expectations.len() {
            let candidate = &self.source_expectations[index];
            let first = conflicting_fingerprint(expectations, candidate)
                .or_else(|| conflicting_owned_fingerprint(&owner, candidate))
                .or_else(|| {
                    self.source_expectations[..index]
                        .iter()
                        .find_map(|existing| conflicting_owned_fingerprint(existing, candidate))
                });
            if let Some(first) = first {
                let candidate = self.source_expectations.swap_remove(index);
                let (locator, second) = candidate.into_parts();
                return Err(ExtractionPlanError::SourceFingerprintConflict {
                    locator,
                    first,
                    second,
                });
            }
        }
        insert_source_expectation(expectations, owner)?;
        for candidate in self.source_expectations.drain(..) {
            insert_source_expectation(expectations, candidate)?;
        }
        let preferred_path =
            allocate_path(self.preferred_content.canonical_extension(), false, budget)?;
        let fallback = self
            .raw_fallback
            .then(|| {
                let content = PlannedContent::RawBinary;
                let path = allocate_path(content.canonical_extension(), true, budget)?;
                PlannedFallback::new(path, content)
                    .map_err(|error| ExtractionPlanError::ModelValidation(error.into()))
            })
            .transpose()?;
        RepresentationContract::from_parts(
            ordinal,
            address,
            RepresentationContractParts {
                preferred_path,
                preferred_content: self.preferred_content,
                fallback,
                working_set_bytes: self.working_set_bytes,
                diagnostics: self.diagnostics,
            },
        )
        .map_err(|error| ExtractionPlanError::ModelValidation(error.into()))
    }
}

trait RepresentationPolicyExt {
    fn raw_fallback(self) -> bool;
}

impl RepresentationPolicyExt for ExtractionRepresentationPolicy {
    fn raw_fallback(self) -> bool {
        self == Self::PreferDecoded
    }
}

fn source_expectation(
    source: &WorkspaceSource,
    budget: &mut AssetLoadBudget,
) -> Result<ExtractionSourceExpectation, ExtractionPlanError> {
    Ok(ExtractionSourceExpectation::new(
        clone_source_locator(
            source.locator(),
            "extraction source expectation locator",
            budget,
        )?,
        source.fingerprint(),
    ))
}

#[cfg(feature = "decode")]
fn streamed_source_expectation(
    resource: &ResolvedStreamedResource,
    budget: &mut AssetLoadBudget,
) -> Result<ExtractionSourceExpectation, ExtractionPlanError> {
    Ok(ExtractionSourceExpectation::new(
        clone_source_locator(
            resource.source().locator(),
            "streamed extraction source expectation locator",
            budget,
        )?,
        resource.source().fingerprint(),
    ))
}

fn insert_source_expectation(
    expectations: &mut Vec<ExtractionSourceExpectation>,
    candidate: ExtractionSourceExpectation,
) -> Result<(), ExtractionPlanError> {
    if let Some(existing) = expectations
        .iter()
        .find(|expectation| expectation.locator() == candidate.locator())
    {
        if existing.fingerprint() != candidate.fingerprint() {
            let (locator, fingerprint) = candidate.into_parts();
            return Err(ExtractionPlanError::SourceFingerprintConflict {
                locator,
                first: existing.fingerprint(),
                second: fingerprint,
            });
        }
        return Ok(());
    }
    expectations.push(candidate);
    Ok(())
}

fn conflicting_fingerprint(
    expectations: &[ExtractionSourceExpectation],
    candidate: &ExtractionSourceExpectation,
) -> Option<SourceFingerprint> {
    expectations
        .iter()
        .find(|expectation| expectation.locator() == candidate.locator())
        .filter(|existing| existing.fingerprint() != candidate.fingerprint())
        .map(ExtractionSourceExpectation::fingerprint)
}

fn conflicting_owned_fingerprint(
    existing: &ExtractionSourceExpectation,
    candidate: &ExtractionSourceExpectation,
) -> Option<SourceFingerprint> {
    (candidate.locator() == existing.locator() && candidate.fingerprint() != existing.fingerprint())
        .then_some(existing.fingerprint())
}

#[cfg(feature = "decode")]
struct ResolvedExtractionStream {
    request: StreamedResourceRequest,
    expectation: ExtractionSourceExpectation,
    bytes: BudgetedMediaBytes,
}

fn raw_content(
    object: &WorkspaceObject,
    budget: &mut AssetLoadBudget,
) -> Result<RepresentationChoice, ExtractionPlanError> {
    match object.value() {
        WorkspaceObjectValue::Binary(_) => Ok(RepresentationChoice::decoded(
            PlannedContent::RawBinary,
            raw_binary_working_set(object).map_err(map_reservation_error)?,
            false,
        )),
        WorkspaceObjectValue::Yaml(_) => Ok(RepresentationChoice::decoded(
            PlannedContent::Yaml,
            yaml_working_set(object, budget).map_err(map_reservation_error)?,
            false,
        )),
    }
}

fn unavailable_choice_with(
    address: &ObjectAddress,
    policy: ExtractionRepresentationPolicy,
    mut raw: RepresentationChoice,
    code: ExtractionDiagnosticCode,
    budget: &mut AssetLoadBudget,
) -> Result<RepresentationChoice, ExtractionPlanError> {
    if policy == ExtractionRepresentationPolicy::RequireDecoded {
        return Err(ExtractionPlanError::RequiredDecodedUnavailable {
            address: clone_object_address(address, "required decoded unavailable address", budget)?,
            reason: code,
        });
    }
    let diagnostic = ExtractionDiagnostic::new(
        code,
        Some(clone_object_address(
            address,
            "extraction diagnostic address",
            budget,
        )?),
    );
    push_budgeted(
        &mut raw.diagnostics,
        diagnostic,
        "extraction diagnostics",
        budget,
    )?;
    Ok(raw)
}

#[cfg(feature = "decode")]
fn decoded_resource_failure(error: &ExtractionPlanError) -> Option<ExtractionDiagnosticCode> {
    match error {
        ExtractionPlanError::MissingStreamResource { .. }
        | ExtractionPlanError::StreamSourceMissing(_)
        | ExtractionPlanError::ObjectUnloaded(_)
        | ExtractionPlanError::ObjectMissing(_)
        | ExtractionPlanError::SourceMissing(_)
        | ExtractionPlanError::Workspace(WorkspaceError::MissingSource(_)) => {
            Some(ExtractionDiagnosticCode::MissingResource)
        }
        ExtractionPlanError::Workspace(WorkspaceError::SourceChanged { .. }) => {
            Some(ExtractionDiagnosticCode::SourceChanged)
        }
        ExtractionPlanError::AmbiguousStreamResource { .. }
        | ExtractionPlanError::ObjectAmbiguous { .. }
        | ExtractionPlanError::ObjectInvalid(_) => {
            Some(ExtractionDiagnosticCode::UnresolvedDependency)
        }
        _ => None,
    }
}

#[cfg(feature = "decode")]
fn sprite_reference_failure(error: &ExtractionPlanError) -> Option<ExtractionDiagnosticCode> {
    match error {
        ExtractionPlanError::ObjectUnloaded(_)
        | ExtractionPlanError::ObjectMissing(_)
        | ExtractionPlanError::ObjectAmbiguous { .. }
        | ExtractionPlanError::ObjectInvalid(_) => {
            Some(ExtractionDiagnosticCode::UnresolvedSpritePPtr)
        }
        _ => decoded_resource_failure(error),
    }
}

fn map_reservation_error(error: ExtractionReservationError) -> ExtractionPlanError {
    match error {
        ExtractionReservationError::Workspace(error) => error.into(),
        ExtractionReservationError::Budget(error) => error.into(),
        #[cfg(feature = "decode")]
        ExtractionReservationError::Reference(error) => error.into(),
        ExtractionReservationError::ObjectUnavailable(address) => {
            ExtractionPlanError::ObjectMissing(address)
        }
        ExtractionReservationError::ArithmeticOverflow { resource } => {
            ExtractionPlanError::ArithmeticOverflow { resource }
        }
        ExtractionReservationError::YamlSizing(message) => ExtractionPlanError::YamlSizing(message),
        error @ ExtractionReservationError::ContentMismatch(_) => {
            ExtractionPlanError::Model(error.to_string())
        }
    }
}

#[cfg(feature = "decode")]
#[derive(Debug)]
enum MediaInspectionOutcome<T> {
    Prepared(T),
    Unavailable(ExtractionDiagnosticCode),
}

#[cfg(feature = "decode")]
fn classify_media_inspection<T>(
    address: &ObjectAddress,
    result: Result<T, MediaInspectionError>,
    budget: &mut AssetLoadBudget,
) -> Result<MediaInspectionOutcome<T>, ExtractionPlanError> {
    match result {
        Ok(layout) => Ok(MediaInspectionOutcome::Prepared(layout)),
        Err(MediaInspectionError::TypeTreeUnavailable) => Ok(MediaInspectionOutcome::Unavailable(
            ExtractionDiagnosticCode::DecodedUnavailable,
        )),
        Err(MediaInspectionError::UnsupportedEncoding { .. }) => Ok(
            MediaInspectionOutcome::Unavailable(ExtractionDiagnosticCode::UnsupportedMediaEncoding),
        ),
        Err(MediaInspectionError::UnsupportedLayout { .. }) => Ok(
            MediaInspectionOutcome::Unavailable(ExtractionDiagnosticCode::UnsupportedMediaLayout),
        ),
        Err(source) => invalid_media_descriptor(address, source, budget),
    }
}

#[cfg(feature = "decode")]
fn invalid_media_descriptor<T>(
    address: &ObjectAddress,
    source: MediaInspectionError,
    budget: &mut AssetLoadBudget,
) -> Result<MediaInspectionOutcome<T>, ExtractionPlanError> {
    Err(ExtractionPlanError::InvalidMediaDescriptor {
        address: clone_object_address(address, "invalid media descriptor address", budget)?,
        source,
    })
}

#[cfg(feature = "decode")]
fn media_preparation_error(
    address: &ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<RepresentationChoice, ExtractionPlanError> {
    Err(ExtractionPlanError::MediaPreparation {
        address: clone_object_address(address, "media preparation error address", budget)?,
    })
}

#[cfg(feature = "decode")]
fn unsupported_audio_choice(
    address: &ObjectAddress,
    policy: ExtractionRepresentationPolicy,
    raw: RepresentationChoice,
    error: AudioSourceError,
    budget: &mut AssetLoadBudget,
) -> Result<RepresentationChoice, ExtractionPlanError> {
    let code = match error {
        AudioSourceError::UnsupportedFormat(_) => {
            ExtractionDiagnosticCode::UnsupportedMediaEncoding
        }
        AudioSourceError::UnsupportedContainer { .. } => {
            ExtractionDiagnosticCode::UnsupportedMediaLayout
        }
        _ => return media_preparation_error(address, budget),
    };
    unavailable_choice_with(address, policy, raw, code, budget)
}

#[cfg(feature = "decode")]
fn resolve_extraction_stream(
    view: &dyn WorkspaceView,
    owner: &WorkspaceSource,
    resolver: &StreamedResourceResolver<'_, '_>,
    stream_path: &str,
    offset: u64,
    size: u64,
    budget: &mut AssetLoadBudget,
) -> Result<ResolvedExtractionStream, ExtractionPlanError> {
    if let Err(error) = StreamedResourceRequest::validate_parts(stream_path, offset, size) {
        return match error {
            StreamedResourceRequestError::RangeOverflow { offset, size } => {
                Err(WorkspaceError::RangeOverflow { offset, size }.into())
            }
            StreamedResourceRequestError::ZeroSize => {
                Err(ExtractionPlanError::InvalidStreamRange { offset, size })
            }
            StreamedResourceRequestError::EmptyPath
            | StreamedResourceRequestError::PathTooLong { .. }
            | StreamedResourceRequestError::ControlCharacter
            | StreamedResourceRequestError::InvalidBasename => {
                Err(ExtractionPlanError::InvalidStreamPath(clone_string(
                    stream_path,
                    "invalid stream path",
                    budget,
                )?))
            }
        };
    }
    let request = StreamedResourceRequest::new(
        clone_source_locator(owner.locator(), "decoded stream owner locator", budget)?,
        clone_string(stream_path, "decoded stream request path", budget)?,
        offset,
        size,
    )
    .map_err(|_| {
        ExtractionPlanError::ReferenceInvariant("validated streamed resource request was rejected")
    })?;
    let resource = match resolver.resolve_request(&request, budget)? {
        StreamedResourceResolution::Resolved { resource } => resource,
        StreamedResourceResolution::Missing => {
            return Err(ExtractionPlanError::MissingStreamResource {
                owner: clone_source_locator(
                    owner.locator(),
                    "missing stream resource owner",
                    budget,
                )?,
                stream_path: clone_string(stream_path, "missing stream resource path", budget)?,
            });
        }
        StreamedResourceResolution::Ambiguous { .. } => {
            return Err(ExtractionPlanError::AmbiguousStreamResource {
                owner: clone_source_locator(
                    owner.locator(),
                    "ambiguous stream resource owner",
                    budget,
                )?,
                stream_path: clone_string(stream_path, "ambiguous stream resource path", budget)?,
            });
        }
        StreamedResourceResolution::OwnerUnloaded | StreamedResourceResolution::OwnerMissing => {
            return Err(ExtractionPlanError::StreamSourceMissing(
                clone_source_locator(
                    owner.locator(),
                    "missing streamed-resource owner locator",
                    budget,
                )?,
            ));
        }
        StreamedResourceResolution::Invalid { .. } => {
            return Err(ExtractionPlanError::InvalidStreamPath(clone_string(
                stream_path,
                "invalid stream path",
                budget,
            )?));
        }
    };
    let expectation = streamed_source_expectation(&resource, budget)?;
    let range = resource.open(view, budget)?;
    let bytes = copy_workspace_range(&range, "planned streamed media", budget)
        .map_err(map_workspace_payload_error)?;
    Ok(ResolvedExtractionStream {
        request,
        expectation,
        bytes,
    })
}

#[cfg(feature = "decode")]
fn materialize_embedded(
    embedded: Option<EmbeddedMediaRef<'_>>,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedMediaBytes, ExtractionPlanError> {
    let embedded = embedded.ok_or(ExtractionPlanError::MediaPayloadChanged { resource })?;
    embedded
        .materialize(resource, budget)
        .map_err(|error| match error {
            EmbeddedMediaError::Budget(error) => ExtractionPlanError::Budget(error),
            EmbeddedMediaError::Allocation {
                resource,
                requested,
                source,
            } => ExtractionPlanError::Allocation {
                resource,
                requested,
                unit: ExtractionAllocationUnit::Bytes,
                source,
            },
            EmbeddedMediaError::EvidenceChanged => {
                ExtractionPlanError::MediaPayloadChanged { resource }
            }
        })
}

#[cfg(feature = "decode")]
fn map_workspace_payload_error(error: WorkspacePayloadError) -> ExtractionPlanError {
    match error {
        WorkspacePayloadError::Budget(error) => ExtractionPlanError::Budget(error),
        WorkspacePayloadError::LengthOverflow { resource } => {
            ExtractionPlanError::ArithmeticOverflow { resource }
        }
        WorkspacePayloadError::Allocation {
            resource,
            requested,
            source,
        } => ExtractionPlanError::Allocation {
            resource,
            requested,
            unit: ExtractionAllocationUnit::Bytes,
            source,
        },
        WorkspacePayloadError::Read { resource, source } => {
            WorkspaceError::operation(resource, source).into()
        }
    }
}

#[cfg(feature = "decode")]
fn resolve_reference_address(
    graph: &ReferenceGraph,
    owner: &RevisionedObjectHandle,
    file_id: i32,
    path_id: i64,
    budget: &mut AssetLoadBudget,
) -> Result<Option<ObjectAddress>, ExtractionPlanError> {
    for fact in graph.outgoing(owner)? {
        let RawReferenceTarget::Binary {
            file_id: candidate_file,
            path_id: candidate_path,
            ..
        } = fact.raw_target()
        else {
            continue;
        };
        if *candidate_file != file_id || *candidate_path != path_id {
            continue;
        }
        if let ReferenceResolution::Resolved(target) = fact.resolution() {
            return Ok(Some(clone_object_address(
                graph.address(target)?,
                "resolved sprite texture address",
                budget,
            )?));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::vec_allocation_bytes;

    #[test]
    fn unavailable_decoding_preserves_the_machine_actionable_reason() {
        let address = ObjectAddress::binary_direct(
            unity_asset_core::SourceLocator::path("media.assets").unwrap(),
            41,
        )
        .unwrap();
        let raw = RepresentationChoice::decoded(PlannedContent::RawBinary, 1, false);

        let mut budget = AssetLoadBudget::default();
        let fallback = unavailable_choice_with(
            &address,
            ExtractionRepresentationPolicy::PreferDecoded,
            raw,
            ExtractionDiagnosticCode::MissingResource,
            &mut budget,
        )
        .unwrap();
        assert_eq!(
            fallback.diagnostics[0].code(),
            ExtractionDiagnosticCode::MissingResource
        );
        let expected_bytes = u64::try_from(address.retained_clone_bytes().unwrap()).unwrap()
            + vec_allocation_bytes::<ExtractionDiagnostic>(fallback.diagnostics.capacity())
                .unwrap();
        assert_eq!(budget.usage().entries, 1);
        assert_eq!(budget.usage().bytes, expected_bytes);

        let required = unavailable_choice_with(
            &address,
            ExtractionRepresentationPolicy::RequireDecoded,
            RepresentationChoice::decoded(PlannedContent::RawBinary, 1, false),
            ExtractionDiagnosticCode::UnresolvedDependency,
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
        assert!(matches!(
            required,
            ExtractionPlanError::RequiredDecodedUnavailable {
                reason: ExtractionDiagnosticCode::UnresolvedDependency,
                ..
            }
        ));
    }

    #[cfg(feature = "decode")]
    #[test]
    fn planner_distinguishes_unavailable_media_from_invalid_typetree_evidence() {
        let address = ObjectAddress::binary_direct(
            unity_asset_core::SourceLocator::path("media.assets").unwrap(),
            41,
        )
        .unwrap();

        for (source, expected) in [
            (
                MediaInspectionError::TypeTreeUnavailable,
                ExtractionDiagnosticCode::DecodedUnavailable,
            ),
            (
                MediaInspectionError::UnsupportedEncoding {
                    family: "Texture2D",
                    value: 10,
                },
                ExtractionDiagnosticCode::UnsupportedMediaEncoding,
            ),
            (
                MediaInspectionError::UnsupportedLayout {
                    family: "Texture2D",
                    layout: "legacy m_MipMap mip layout",
                },
                ExtractionDiagnosticCode::UnsupportedMediaLayout,
            ),
        ] {
            let outcome = classify_media_inspection::<()>(
                &address,
                Err(source),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
            assert!(matches!(
                outcome,
                MediaInspectionOutcome::Unavailable(actual) if actual == expected
            ));
        }

        for source in [
            MediaInspectionError::InvalidDescriptor {
                field: "m_Width",
                reason: "field is malformed",
            },
            MediaInspectionError::MissingPayload,
            MediaInspectionError::AmbiguousPayload,
            MediaInspectionError::StreamRangeOverflow {
                offset: u64::MAX,
                size: 1,
            },
        ] {
            let error = classify_media_inspection::<()>(
                &address,
                Err(source),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
            assert!(matches!(
                error,
                ExtractionPlanError::InvalidMediaDescriptor { .. }
            ));
        }
    }

    #[cfg(feature = "decode")]
    #[test]
    fn planner_distinguishes_unsupported_audio_encoding_from_container_layout() {
        use unity_asset_decode::audio::AudioCompressionFormat;

        let address = ObjectAddress::binary_direct(
            unity_asset_core::SourceLocator::path("media.assets").unwrap(),
            41,
        )
        .unwrap();

        for (source, expected) in [
            (
                AudioSourceError::UnsupportedFormat(AudioCompressionFormat::Unknown),
                ExtractionDiagnosticCode::UnsupportedMediaEncoding,
            ),
            (
                AudioSourceError::UnsupportedContainer {
                    format: AudioCompressionFormat::Vorbis,
                    container: "Ogg Vorbis",
                },
                ExtractionDiagnosticCode::UnsupportedMediaLayout,
            ),
        ] {
            let raw = RepresentationChoice::decoded(PlannedContent::RawBinary, 1, false);
            let error = unsupported_audio_choice(
                &address,
                ExtractionRepresentationPolicy::RequireDecoded,
                raw,
                source,
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
            assert!(matches!(
                error,
                ExtractionPlanError::RequiredDecodedUnavailable { reason, .. }
                    if reason == expected
            ));
        }
    }

    #[cfg(feature = "decode")]
    #[test]
    fn decoded_resource_failures_never_downgrade_invalid_paths_or_ranges() {
        let owner = unity_asset_core::SourceLocator::path("media.assets").unwrap();
        let missing = ExtractionPlanError::MissingStreamResource {
            owner: owner.clone(),
            stream_path: "missing.resS".to_owned(),
        };
        let invalid = ExtractionPlanError::InvalidStreamPath(".".to_owned());
        let owner_missing = ExtractionPlanError::StreamSourceMissing(owner.clone());
        let ambiguous_stream = ExtractionPlanError::AmbiguousStreamResource {
            owner: owner.clone(),
            stream_path: "shared.resS".to_owned(),
        };
        let resource_source = unity_asset_core::SourceId::new(
            unity_asset_core::WorkspaceId::from_u128(1).unwrap(),
            unity_asset_core::SourceKind::StreamedResource,
            1,
        )
        .unwrap();
        let out_of_bounds = ExtractionPlanError::Workspace(WorkspaceError::RangeOutOfBounds {
            source_id: resource_source,
            offset: 2,
            end: 5,
            source_len: 4,
        });
        let address = ObjectAddress::binary_direct(owner, 41).unwrap();
        let unresolved = ExtractionPlanError::ObjectAmbiguous {
            address,
            candidates: 2,
        };

        assert_eq!(
            decoded_resource_failure(&missing),
            Some(ExtractionDiagnosticCode::MissingResource)
        );
        assert_eq!(decoded_resource_failure(&invalid), None);
        assert_eq!(
            decoded_resource_failure(&owner_missing),
            Some(ExtractionDiagnosticCode::MissingResource)
        );
        assert_eq!(decoded_resource_failure(&out_of_bounds), None);
        assert_eq!(
            decoded_resource_failure(&ambiguous_stream),
            Some(ExtractionDiagnosticCode::UnresolvedDependency)
        );
        assert_eq!(
            decoded_resource_failure(&unresolved),
            Some(ExtractionDiagnosticCode::UnresolvedDependency)
        );
        assert_eq!(
            sprite_reference_failure(&unresolved),
            Some(ExtractionDiagnosticCode::UnresolvedSpritePPtr)
        );
    }
}
