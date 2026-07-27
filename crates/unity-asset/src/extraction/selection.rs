use std::fmt::Write as _;
use std::io::{self, Write};

use thiserror::Error;
use unity_asset_binary::asset::class_ids;
#[cfg(feature = "decode")]
use unity_asset_binary::object::UnityObject;
#[cfg(feature = "decode")]
use unity_asset_binary::unity_version::UnityVersion;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ContractError, ObjectAddress, ObjectKind, RevisionedObjectHandle,
    SourceFingerprint, SourceId, SourceLocator, UnityValue, vec_allocation_bytes,
};
use unity_asset_yaml::UnityYamlSerializer;

#[cfg(feature = "decode")]
use unity_asset_decode::{
    audio::{AudioClipLayout, AudioCompressionFormat, MAX_VORBIS_SETUP_PACKET_BYTES},
    sprite::SpriteTextureReference,
    texture::Texture2DLayout,
};

use super::container::{
    BundleContainerQuery, BundleContainerResult, query_bundle_container_occurrences,
    resolved_addresses,
};
use super::manifest::canonical_digest;
#[cfg(feature = "decode")]
use super::model::ExtractionSourceRange;
use super::model::{
    ExtractionArtifactKind, ExtractionDiagnostic, ExtractionDiagnosticCode, ExtractionModelError,
    ExtractionPath, ExtractionPlan, ExtractionRepresentationPolicy, ExtractionRequest,
    ExtractionSelection, ExtractionSourceExpectation, PlannedArtifact, PlannedContent,
};
use super::source_budget_error;
#[cfg(feature = "decode")]
use crate::reference::{RawReferenceTarget, ReferenceResolution};
use crate::reference::{ReferenceGraph, ReferenceGraphError, ReferenceTraversal};
#[cfg(feature = "decode")]
use crate::workspace::{
    ResolvedStreamedResource, StreamedResourceRequest, StreamedResourceRequestError,
    StreamedResourceResolution,
};
use crate::workspace::{
    StreamedResourceResolver, WorkspaceError, WorkspaceLookup, WorkspaceObject,
    WorkspaceObjectValue, WorkspaceSource, WorkspaceView,
};

/// Plans deterministic extraction artifacts against one immutable workspace view.
pub struct ExtractionPlanner<'view> {
    view: &'view dyn WorkspaceView,
    references: Option<&'view ReferenceGraph>,
}

impl<'view> ExtractionPlanner<'view> {
    #[must_use]
    pub const fn new(view: &'view dyn WorkspaceView) -> Self {
        Self {
            view,
            references: None,
        }
    }

    #[must_use]
    pub const fn with_reference_graph(mut self, references: &'view ReferenceGraph) -> Self {
        self.references = Some(references);
        self
    }

    pub fn plan(
        &self,
        request: ExtractionRequest,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        self.validate_reference_context()?;

        let sources = self.view.sources(budget)?;
        let stream_resolver = if cfg!(feature = "decode")
            && request.representation() != ExtractionRepresentationPolicy::RawOnly
        {
            Some(StreamedResourceResolver::new(self.view, &sources, budget)?)
        } else {
            None
        };
        let handles = selected_handles(self.view, &request, &sources, budget)?;
        let mut candidates = budgeted_vec(handles.len(), "extraction candidate addresses", budget)?;
        for handle in handles {
            let address = self.view.object_address(&handle, budget)?;
            candidates.push((address, handle));
        }
        candidates.sort_by(|left, right| left.0.cmp(&right.0));
        candidates.dedup_by(|left, right| left.0 == right.0);

        let mut source_expectations =
            budgeted_vec(sources.len(), "extraction source expectations", budget)?;
        let mut artifacts = budgeted_vec(candidates.len(), "extraction planned artifacts", budget)?;
        for (address, handle) in candidates {
            let object = self.view.read_object(&handle, budget)?;
            if !request
                .filter()
                .matches_class(object.class().class_id(), object.class().class_name())
            {
                continue;
            }
            let object_name = object_name(&object, budget)?;
            if !request.filter().matches_object_name(object_name.as_deref()) {
                continue;
            }
            if request.filter().limit().is_some_and(|limit| {
                u64::try_from(artifacts.len()).is_ok_and(|count| count >= limit)
            }) {
                break;
            }

            let owner = source_for_id(handle.object().source(), &sources)?;
            insert_source_expectation(
                &mut source_expectations,
                SourceExpectationOwned::from_source(owner, budget)?,
            )?;
            let choice = self.plan_content(
                &address,
                &object,
                owner,
                &sources,
                stream_resolver.as_ref(),
                request.representation(),
                budget,
            )?;
            for expectation in choice.source_expectations {
                insert_source_expectation(&mut source_expectations, expectation)?;
            }

            let ordinal = u32::try_from(artifacts.len()).map_err(|_| {
                ExtractionPlanError::ArithmeticOverflow {
                    resource: "extraction artifact ordinal",
                }
            })?;
            let preferred_path = allocate_path(
                request.prefix(),
                &address,
                owner.locator(),
                object.class().class_id(),
                object.class().class_name(),
                object_name.as_deref(),
                choice.preferred_extension,
                false,
                budget,
            )?;
            let fallback = match choice.fallback {
                Some((kind, content)) => Some((
                    kind,
                    allocate_path(
                        request.prefix(),
                        &address,
                        owner.locator(),
                        object.class().class_id(),
                        object.class().class_name(),
                        object_name.as_deref(),
                        "bin",
                        true,
                        budget,
                    )?,
                    content,
                )),
                None => None,
            };
            let class_name = clone_string(
                object.class().class_name(),
                "extraction artifact class name",
                budget,
            )?;
            artifacts.push(
                PlannedArtifact::new(
                    ordinal,
                    address,
                    object.class().class_id(),
                    class_name,
                    object_name,
                    choice.preferred_kind,
                    preferred_path,
                    choice.preferred_content,
                    fallback,
                    choice.working_set_bytes,
                    choice.diagnostics,
                )
                .map_err(map_model_error)?,
            );
        }

        source_expectations.sort_unstable_by(|left, right| left.locator().cmp(right.locator()));
        ExtractionPlan::new_budgeted(
            self.view.workspace_id(),
            self.view.revision(),
            request,
            source_expectations,
            artifacts,
            budget,
        )
        .map_err(map_model_error)
    }

    pub fn plan_handles(
        &self,
        handles: &[RevisionedObjectHandle],
        representation: ExtractionRepresentationPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        let mut addresses = budgeted_vec(handles.len(), "extraction handle addresses", budget)?;
        for handle in handles {
            handle.validate_context(self.view.workspace_id(), self.view.revision())?;
            addresses.push(self.view.object_address(handle, budget)?);
        }
        let request = ExtractionRequest::addresses(addresses, representation)
            .map_err(|error| ExtractionPlanError::Model(error.to_string()))?;
        self.plan(request, budget)
    }

    pub fn plan_traversal(
        &self,
        traversal: &ReferenceTraversal,
        representation: ExtractionRepresentationPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        if traversal.workspace_id() != self.view.workspace_id()
            || traversal.revision() != self.view.revision()
        {
            return Err(ExtractionPlanError::ReferenceContextMismatch);
        }
        if !traversal.is_complete() {
            return Err(ExtractionPlanError::IncompleteReferenceTraversal);
        }
        let mut addresses =
            budgeted_vec(traversal.len(), "extraction traversal addresses", budget)?;
        for handle in traversal.nodes() {
            handle.validate_context(self.view.workspace_id(), self.view.revision())?;
            addresses.push(self.view.object_address(handle, budget)?);
        }
        let request = ExtractionRequest::addresses(addresses, representation)
            .map_err(|error| ExtractionPlanError::Model(error.to_string()))?;
        self.plan(request, budget)
    }

    pub fn plan_bundle_containers(
        &self,
        pattern: &str,
        representation: ExtractionRepresentationPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        let pattern = clone_string(pattern, "bundle container query pattern", budget)?;
        let query = BundleContainerQuery::new(pattern)?;
        let result = self.bundle_container_occurrences(query, budget)?;
        if !result.is_complete() {
            return Err(ExtractionPlanError::IncompleteReferenceGraph);
        }
        let addresses = resolved_addresses(&result, budget)?;
        let request_pattern = clone_string(
            result.query().pattern(),
            "bundle container extraction pattern",
            budget,
        )?;
        let request =
            ExtractionRequest::bundle_container(request_pattern, addresses, representation)
                .map_err(|error| ExtractionPlanError::Model(error.to_string()))?;
        self.plan(request, budget)
    }

    /// Returns every exact `AssetBundle.m_Container` reference occurrence without deduplication.
    pub fn bundle_container_occurrences(
        &self,
        query: BundleContainerQuery,
        budget: &mut AssetLoadBudget,
    ) -> Result<BundleContainerResult, ExtractionPlanError> {
        self.validate_reference_context()?;
        let references = self
            .references
            .ok_or(ExtractionPlanError::ReferenceGraphRequired)?;
        query_bundle_container_occurrences(self.view, references, query, budget)
    }

    fn validate_reference_context(&self) -> Result<(), ExtractionPlanError> {
        if let Some(references) = self.references
            && (references.workspace_id() != self.view.workspace_id()
                || references.revision() != self.view.revision())
        {
            return Err(ExtractionPlanError::ReferenceContextMismatch);
        }
        Ok(())
    }

    fn plan_content(
        &self,
        address: &ObjectAddress,
        object: &WorkspaceObject,
        _owner: &WorkspaceSource,
        _sources: &[WorkspaceSource],
        _stream_resolver: Option<&StreamedResourceResolver<'_, '_>>,
        policy: ExtractionRepresentationPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<ContentChoice, ExtractionPlanError> {
        #[cfg(feature = "decode")]
        let (owner, sources, stream_resolver) = (_owner, _sources, _stream_resolver);
        let raw = raw_content(object, budget)?;
        if policy == ExtractionRepresentationPolicy::RawOnly
            || matches!(object.value(), WorkspaceObjectValue::Yaml(_))
        {
            return Ok(raw);
        }

        let WorkspaceObjectValue::Binary(binary) = object.value() else {
            return Ok(raw);
        };
        if binary.class_id() == class_ids::TEXT_ASSET {
            return Ok(ContentChoice::decoded(
                ExtractionArtifactKind::Text,
                "txt",
                PlannedContent::TextAsset,
                usize_to_u64(binary.payload_len(), "text asset working set")?,
                policy.then_fallback(raw),
            ));
        }

        #[cfg(not(feature = "decode"))]
        {
            unavailable_choice(address, policy, raw, budget)
        }

        #[cfg(feature = "decode")]
        {
            let stream_resolver = stream_resolver.ok_or_else(|| {
                WorkspaceError::operation(
                    "extraction streamed-resource resolver",
                    io::Error::other("decoded extraction requires a streamed-resource index"),
                )
            })?;
            let Some(version) = object
                .schema_provenance()
                .binary_version()
                .and_then(|version| version.unity())
            else {
                return unavailable_choice(address, policy, raw, budget);
            };

            match binary.class_id() {
                class_ids::AUDIO_CLIP => {
                    let layout = match AudioClipLayout::inspect(binary, version) {
                        Ok(layout) => layout,
                        Err(_) => return unavailable_choice(address, policy, raw, budget),
                    };
                    let (stream, stream_expectation) = if let Some(stream) =
                        layout.payload().stream()
                    {
                        match resolve_extraction_stream(
                            owner,
                            stream_resolver,
                            stream.path(),
                            stream.offset(),
                            u64::from(stream.size()),
                            budget,
                        ) {
                            Ok(resolved) => (Some(resolved.range), Some(resolved.expectation)),
                            Err(error) => {
                                let Some(code) = decoded_resource_failure(&error) else {
                                    return Err(error);
                                };
                                return unavailable_choice_with(address, policy, raw, code, budget);
                            }
                        }
                    } else {
                        (None, None)
                    };
                    let extension = clone_string(
                        layout.compression_format().extension(),
                        "audio output extension",
                        budget,
                    )?;
                    let encoded_audio_bytes = match stream.as_ref() {
                        Some(stream) => stream.size(),
                        None => usize_to_u64(
                            layout
                                .payload()
                                .embedded_byte_len()
                                .expect("non-streamed layout is embedded"),
                            "embedded audio size",
                        )?,
                    };
                    let output_bound =
                        if layout.compression_format() == AudioCompressionFormat::Vorbis {
                            ogg_output_bound(encoded_audio_bytes)?
                        } else {
                            encoded_audio_bytes
                        };
                    let working_set = checked_sum(
                        [
                            usize_to_u64(binary.payload_len(), "audio working set")?,
                            stream.as_ref().map_or(0, ExtractionSourceRange::size),
                            if stream.is_none() {
                                encoded_audio_bytes
                            } else {
                                0
                            },
                            output_bound,
                        ],
                        "audio working set",
                    )?;
                    let mut expectations = budgeted_vec(
                        if stream.is_some() { 1 } else { 0 },
                        "audio source expectations",
                        budget,
                    )?;
                    if let Some(expectation) = stream_expectation {
                        expectations.push(expectation);
                    }
                    let version = clone_unity_version(version, budget)?;
                    Ok(ContentChoice::decoded_with_sources(
                        ExtractionArtifactKind::Audio,
                        layout.compression_format().extension(),
                        PlannedContent::Audio {
                            version,
                            extension,
                            stream,
                        },
                        working_set,
                        policy.then_fallback(raw),
                        expectations,
                    ))
                }
                class_ids::TEXTURE_2D => {
                    let layout = match Texture2DLayout::inspect(binary) {
                        Ok(layout) => layout,
                        Err(_) => return unavailable_choice(address, policy, raw, budget),
                    };
                    let (stream, stream_expectation) = if let Some(stream) =
                        layout.payload().stream()
                    {
                        match resolve_extraction_stream(
                            owner,
                            stream_resolver,
                            stream.path(),
                            stream.offset(),
                            u64::from(stream.size()),
                            budget,
                        ) {
                            Ok(resolved) => (Some(resolved.range), Some(resolved.expectation)),
                            Err(error) => {
                                let Some(code) = decoded_resource_failure(&error) else {
                                    return Err(error);
                                };
                                return unavailable_choice_with(address, policy, raw, code, budget);
                            }
                        }
                    } else {
                        (None, None)
                    };
                    let image_bytes = u64::try_from(layout.width())
                        .ok()
                        .and_then(|width| {
                            u64::try_from(layout.height())
                                .ok()
                                .and_then(|height| width.checked_mul(height))
                        })
                        .and_then(|pixels| pixels.checked_mul(4))
                        .ok_or(ExtractionPlanError::ArithmeticOverflow {
                            resource: "texture working set",
                        })?;
                    let mut expectations = budgeted_vec(
                        if stream.is_some() { 1 } else { 0 },
                        "texture source expectations",
                        budget,
                    )?;
                    if let Some(expectation) = stream_expectation {
                        expectations.push(expectation);
                    }
                    let stream_bytes = stream.as_ref().map_or(0, ExtractionSourceRange::size);
                    let embedded_bytes = layout
                        .payload()
                        .embedded_byte_len()
                        .map(|size| usize_to_u64(size, "embedded texture size"))
                        .transpose()?
                        .unwrap_or(0);
                    let version = clone_unity_version(version, budget)?;
                    Ok(ContentChoice::decoded_with_sources(
                        ExtractionArtifactKind::TexturePng,
                        "png",
                        PlannedContent::TexturePng { version, stream },
                        checked_sum(
                            [
                                image_bytes,
                                png_output_bound(image_bytes)?,
                                usize_to_u64(binary.payload_len(), "texture working set")?,
                                stream_bytes,
                                embedded_bytes,
                            ],
                            "texture working set",
                        )?,
                        policy.then_fallback(raw),
                        expectations,
                    ))
                }
                class_ids::SPRITE => self.plan_sprite(
                    address,
                    binary,
                    owner,
                    sources,
                    stream_resolver,
                    version,
                    policy,
                    raw,
                    budget,
                ),
                _ => unavailable_choice_with(
                    address,
                    policy,
                    raw,
                    ExtractionDiagnosticCode::UnsupportedClass,
                    budget,
                ),
            }
        }
    }

    #[cfg(feature = "decode")]
    #[allow(clippy::too_many_arguments)]
    fn plan_sprite(
        &self,
        address: &ObjectAddress,
        binary: &UnityObject,
        owner: &WorkspaceSource,
        sources: &[WorkspaceSource],
        stream_resolver: &StreamedResourceResolver<'_, '_>,
        version: &UnityVersion,
        policy: ExtractionRepresentationPolicy,
        raw: ContentChoice,
        budget: &mut AssetLoadBudget,
    ) -> Result<ContentChoice, ExtractionPlanError> {
        let texture_reference = match SpriteTextureReference::inspect(binary) {
            Ok(reference) => reference,
            Err(_) => return unavailable_choice(address, policy, raw, budget),
        };
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
                    policy,
                    raw,
                    ExtractionDiagnosticCode::UnresolvedDependency,
                    budget,
                );
            };
            let owner_handle = resolve_required_handle(self.view, address, budget)?;
            let Some(texture) =
                resolve_reference_address(references, &owner_handle, file_id, path_id, budget)?
            else {
                return unavailable_choice_with(
                    address,
                    policy,
                    raw,
                    ExtractionDiagnosticCode::UnresolvedDependency,
                    budget,
                );
            };
            texture
        };
        let texture_handle = match resolve_required_handle(self.view, &texture_address, budget) {
            Ok(handle) => handle,
            Err(error) => {
                let Some(code) = decoded_resource_failure(&error) else {
                    return Err(error);
                };
                return unavailable_choice_with(address, policy, raw, code, budget);
            }
        };
        let texture_object = self.view.read_object(&texture_handle, budget)?;
        let WorkspaceObjectValue::Binary(texture_binary) = texture_object.value() else {
            return unavailable_choice_with(
                address,
                policy,
                raw,
                ExtractionDiagnosticCode::UnresolvedDependency,
                budget,
            );
        };
        if texture_binary.class_id() != class_ids::TEXTURE_2D {
            return unavailable_choice_with(
                address,
                policy,
                raw,
                ExtractionDiagnosticCode::UnresolvedDependency,
                budget,
            );
        }
        let texture_owner = match source_for_id(texture_handle.object().source(), sources) {
            Ok(owner) => owner,
            Err(error) => {
                let Some(code) = decoded_resource_failure(&error) else {
                    return Err(error);
                };
                return unavailable_choice_with(address, policy, raw, code, budget);
            }
        };
        let texture_layout = match Texture2DLayout::inspect(texture_binary) {
            Ok(layout) => layout,
            Err(_) => return unavailable_choice(address, policy, raw, budget),
        };
        let (texture_stream, stream_expectation) =
            if let Some(stream) = texture_layout.payload().stream() {
                match resolve_extraction_stream(
                    texture_owner,
                    stream_resolver,
                    stream.path(),
                    stream.offset(),
                    u64::from(stream.size()),
                    budget,
                ) {
                    Ok(resolved) => (Some(resolved.range), Some(resolved.expectation)),
                    Err(error) => {
                        let Some(code) = decoded_resource_failure(&error) else {
                            return Err(error);
                        };
                        return unavailable_choice_with(address, policy, raw, code, budget);
                    }
                }
            } else {
                (None, None)
            };
        let mut expectations = budgeted_vec(
            if texture_stream.is_some() { 2 } else { 1 },
            "sprite source expectations",
            budget,
        )?;
        expectations.push(SourceExpectationOwned::from_source(texture_owner, budget)?);
        if let Some(expectation) = stream_expectation {
            expectations.push(expectation);
        }
        let texture_stream_bytes = texture_stream
            .as_ref()
            .map_or(0, ExtractionSourceRange::size);
        let embedded_texture_bytes = texture_layout
            .payload()
            .embedded_byte_len()
            .map(|size| usize_to_u64(size, "embedded sprite texture size"))
            .transpose()?
            .unwrap_or(0);
        let image_bytes = u64::try_from(texture_layout.width())
            .ok()
            .and_then(|width| {
                u64::try_from(texture_layout.height())
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(ExtractionPlanError::ArithmeticOverflow {
                resource: "sprite working set",
            })?;
        let version = clone_unity_version(version, budget)?;
        Ok(ContentChoice::decoded_with_sources(
            ExtractionArtifactKind::SpritePng,
            "png",
            PlannedContent::SpritePng {
                version,
                texture: texture_address,
                texture_stream,
            },
            checked_sum(
                [
                    image_bytes,
                    png_output_bound(image_bytes)?,
                    usize_to_u64(binary.payload_len(), "sprite working set")?,
                    usize_to_u64(texture_binary.payload_len(), "sprite working set")?,
                    texture_stream_bytes,
                    embedded_texture_bytes,
                ],
                "sprite working set",
            )?,
            policy.then_fallback(raw),
            expectations,
        ))
    }
}

#[derive(Debug)]
struct ContentChoice {
    preferred_kind: ExtractionArtifactKind,
    preferred_extension: &'static str,
    preferred_content: PlannedContent,
    fallback: Option<(ExtractionArtifactKind, PlannedContent)>,
    working_set_bytes: u64,
    diagnostics: Vec<ExtractionDiagnostic>,
    source_expectations: Vec<SourceExpectationOwned>,
}

impl ContentChoice {
    fn decoded(
        kind: ExtractionArtifactKind,
        extension: &'static str,
        content: PlannedContent,
        working_set_bytes: u64,
        fallback: Option<(ExtractionArtifactKind, PlannedContent)>,
    ) -> Self {
        Self::decoded_with_sources(
            kind,
            extension,
            content,
            working_set_bytes,
            fallback,
            Vec::new(),
        )
    }

    fn decoded_with_sources(
        kind: ExtractionArtifactKind,
        extension: &'static str,
        content: PlannedContent,
        working_set_bytes: u64,
        fallback: Option<(ExtractionArtifactKind, PlannedContent)>,
        source_expectations: Vec<SourceExpectationOwned>,
    ) -> Self {
        Self {
            preferred_kind: kind,
            preferred_extension: extension,
            preferred_content: content,
            fallback,
            working_set_bytes: working_set_bytes.max(1),
            diagnostics: Vec::new(),
            source_expectations,
        }
    }
}

trait RepresentationPolicyExt {
    fn then_fallback(self, raw: ContentChoice) -> Option<(ExtractionArtifactKind, PlannedContent)>;
}

impl RepresentationPolicyExt for ExtractionRepresentationPolicy {
    fn then_fallback(self, raw: ContentChoice) -> Option<(ExtractionArtifactKind, PlannedContent)> {
        (self == Self::PreferDecoded).then_some((raw.preferred_kind, raw.preferred_content))
    }
}

#[derive(Debug)]
struct SourceExpectationOwned {
    locator: SourceLocator,
    fingerprint: unity_asset_core::SourceFingerprint,
}

impl SourceExpectationOwned {
    fn from_source(
        source: &WorkspaceSource,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ExtractionPlanError> {
        Ok(Self {
            locator: clone_source_locator(
                source.locator(),
                "extraction source expectation locator",
                budget,
            )?,
            fingerprint: source.fingerprint(),
        })
    }

    #[cfg(feature = "decode")]
    fn from_streamed_resource(
        resource: &ResolvedStreamedResource,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ExtractionPlanError> {
        Ok(Self {
            locator: clone_source_locator(
                resource.source().locator(),
                "streamed extraction source expectation locator",
                budget,
            )?,
            fingerprint: resource.source().fingerprint(),
        })
    }
}

#[cfg(feature = "decode")]
struct ResolvedExtractionStream {
    range: ExtractionSourceRange,
    expectation: SourceExpectationOwned,
}

fn insert_source_expectation(
    expectations: &mut Vec<ExtractionSourceExpectation>,
    candidate: SourceExpectationOwned,
) -> Result<(), ExtractionPlanError> {
    if let Some(existing) = expectations
        .iter()
        .find(|expectation| expectation.locator() == &candidate.locator)
    {
        if existing.fingerprint() != candidate.fingerprint {
            return Err(ExtractionPlanError::SourceFingerprintConflict {
                locator: candidate.locator,
                first: existing.fingerprint(),
                second: candidate.fingerprint,
            });
        }
        return Ok(());
    }
    expectations.push(ExtractionSourceExpectation::new(
        candidate.locator,
        candidate.fingerprint,
    ));
    Ok(())
}

fn raw_content(
    object: &WorkspaceObject,
    budget: &mut AssetLoadBudget,
) -> Result<ContentChoice, ExtractionPlanError> {
    match object.value() {
        WorkspaceObjectValue::Binary(binary) => Ok(ContentChoice::decoded(
            ExtractionArtifactKind::BinaryRaw,
            "bin",
            PlannedContent::RawBinary,
            usize_to_u64(binary.payload_len(), "raw object length")?,
            None,
        )),
        WorkspaceObjectValue::Yaml(_) => {
            let mut counter = PlanningByteCounter::default();
            if let Err(error) = UnityYamlSerializer::new().serialize_to_writer_with_budget(
                &mut counter,
                std::iter::once(object.class()),
                budget,
            ) {
                if let Some(error) = source_budget_error(&error) {
                    return Err(error.clone().into());
                }
                return Err(ExtractionPlanError::YamlSizing(error.to_string()));
            }
            Ok(ContentChoice::decoded(
                ExtractionArtifactKind::Yaml,
                "yaml",
                PlannedContent::Yaml,
                counter.bytes,
                None,
            ))
        }
    }
}

fn unavailable_choice(
    address: &ObjectAddress,
    policy: ExtractionRepresentationPolicy,
    raw: ContentChoice,
    budget: &mut AssetLoadBudget,
) -> Result<ContentChoice, ExtractionPlanError> {
    unavailable_choice_with(
        address,
        policy,
        raw,
        ExtractionDiagnosticCode::DecodedUnavailable,
        budget,
    )
}

fn unavailable_choice_with(
    address: &ObjectAddress,
    policy: ExtractionRepresentationPolicy,
    mut raw: ContentChoice,
    code: ExtractionDiagnosticCode,
    budget: &mut AssetLoadBudget,
) -> Result<ContentChoice, ExtractionPlanError> {
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
        ExtractionPlanError::InvalidStreamPath(_)
        | ExtractionPlanError::MissingStreamResource { .. }
        | ExtractionPlanError::StreamSourceMissing(_)
        | ExtractionPlanError::ObjectUnloaded(_)
        | ExtractionPlanError::ObjectMissing(_)
        | ExtractionPlanError::SourceMissing(_)
        | ExtractionPlanError::Workspace(
            WorkspaceError::MissingSource(_) | WorkspaceError::RangeOutOfBounds { .. },
        ) => Some(ExtractionDiagnosticCode::MissingResource),
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

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, ExtractionPlanError> {
    u64::try_from(value).map_err(|_| ExtractionPlanError::ArithmeticOverflow { resource })
}

fn clone_string(
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ExtractionPlanError> {
    let bytes = usize_to_u64(value.len(), resource)?;
    budget.check_bytes(bytes)?;
    let mut cloned = String::new();
    cloned
        .try_reserve_exact(value.len())
        .map_err(|source| ExtractionPlanError::Allocation {
            resource,
            requested: value.len(),
            source,
        })?;
    cloned.push_str(value);
    budget.consume_bytes(bytes)?;
    Ok(cloned)
}

fn clone_source_locator(
    value: &SourceLocator,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<SourceLocator, ExtractionPlanError> {
    let bytes = value
        .retained_clone_bytes()
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let cloned = value.clone();
    budget.consume_bytes(bytes)?;
    Ok(cloned)
}

fn clone_object_address(
    value: &ObjectAddress,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<ObjectAddress, ExtractionPlanError> {
    let bytes = value
        .retained_clone_bytes()
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let cloned = value.clone();
    budget.consume_bytes(bytes)?;
    Ok(cloned)
}

#[cfg(feature = "decode")]
fn clone_unity_version(
    value: &UnityVersion,
    budget: &mut AssetLoadBudget,
) -> Result<UnityVersion, ExtractionPlanError> {
    Ok(UnityVersion {
        major: value.major,
        minor: value.minor,
        build: value.build,
        version_type: value.version_type,
        type_number: value.type_number,
        type_str: value
            .type_str
            .as_deref()
            .map(|channel| clone_string(channel, "planned Unity version channel", budget))
            .transpose()?,
    })
}

fn map_model_error(error: ExtractionModelError) -> ExtractionPlanError {
    match error {
        ExtractionModelError::Budget(source) => ExtractionPlanError::Budget(source),
        ExtractionModelError::InvalidPath(
            unity_asset_write::artifact::ArtifactNameError::Budget(source),
        ) => ExtractionPlanError::Budget(source),
        ExtractionModelError::InvalidPath(
            unity_asset_write::artifact::ArtifactNameError::Allocation {
                resource,
                requested,
                source,
            },
        ) => ExtractionPlanError::Allocation {
            resource,
            requested,
            source,
        },
        ExtractionModelError::Allocation {
            resource,
            requested,
            source,
        } => ExtractionPlanError::Allocation {
            resource,
            requested,
            source,
        },
        other => ExtractionPlanError::Model(other.to_string()),
    }
}

fn budgeted_vec<T>(
    count: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, ExtractionPlanError> {
    let entries = u64::try_from(count).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    let minimum_bytes = vec_allocation_bytes::<T>(count)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_entries(entries)?;
    budget.check_bytes(minimum_bytes)?;

    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|source| ExtractionPlanError::Allocation {
            resource,
            requested: count,
            source,
        })?;
    let retained_bytes = vec_allocation_bytes::<T>(values.capacity())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(values)
}

fn push_budgeted<T>(
    values: &mut Vec<T>,
    value: T,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), ExtractionPlanError> {
    budget.check_entries(1)?;
    let previous_bytes = vec_allocation_bytes::<T>(values.capacity())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    if values.len() == values.capacity() {
        let planned_capacity = values
            .capacity()
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow { resource })?;
        let planned_bytes = vec_allocation_bytes::<T>(planned_capacity)
            .map_err(|_| BudgetError::ArithmeticOverflow { resource })?
            .checked_sub(previous_bytes)
            .ok_or(BudgetError::ArithmeticOverflow { resource })?;
        budget.check_bytes(planned_bytes)?;
        values
            .try_reserve_exact(1)
            .map_err(|source| ExtractionPlanError::Allocation {
                resource,
                requested: 1,
                source,
            })?;
    }
    let retained_bytes = vec_allocation_bytes::<T>(values.capacity())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?
        .checked_sub(previous_bytes)
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_entries(1)?;
    budget.consume_bytes(retained_bytes)?;
    values.push(value);
    Ok(())
}

#[cfg(feature = "decode")]
fn checked_sum(
    values: impl IntoIterator<Item = u64>,
    resource: &'static str,
) -> Result<u64, ExtractionPlanError> {
    values.into_iter().try_fold(0_u64, |total, value| {
        total
            .checked_add(value)
            .ok_or(ExtractionPlanError::ArithmeticOverflow { resource })
    })
}

#[cfg(feature = "decode")]
fn png_output_bound(rgba_bytes: u64) -> Result<u64, ExtractionPlanError> {
    rgba_bytes
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(1024 * 1024))
        .ok_or(ExtractionPlanError::ArithmeticOverflow {
            resource: "PNG output bound",
        })
}

#[cfg(feature = "decode")]
fn ogg_output_bound(encoded_bytes: u64) -> Result<u64, ExtractionPlanError> {
    // An FSB5 packet needs at least a two-byte length and one data byte. Even
    // if every packet occupies its own Ogg page, sixteen times the input covers
    // payload, lacing values, and page headers. The setup packet is independently
    // capped by the decoder module and included as a fixed component.
    encoded_bytes
        .checked_mul(16)
        .and_then(|bytes| {
            MAX_VORBIS_SETUP_PACKET_BYTES
                .checked_mul(2)
                .and_then(|fixed| bytes.checked_add(fixed))
        })
        .and_then(|bytes| bytes.checked_add(64 * 1024))
        .ok_or(ExtractionPlanError::ArithmeticOverflow {
            resource: "Ogg output bound",
        })
}

#[derive(Default)]
struct PlanningByteCounter {
    bytes: u64,
}

impl Write for PlanningByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(u64::try_from(buffer.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| io::Error::other("planned YAML output length overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn selected_handles(
    view: &dyn WorkspaceView,
    request: &ExtractionRequest,
    sources: &[WorkspaceSource],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<RevisionedObjectHandle>, ExtractionPlanError> {
    match request.selection() {
        ExtractionSelection::All => view.objects(budget).map_err(Into::into),
        ExtractionSelection::Sources { sources: selected } => {
            let mut handles = view.objects(budget)?;
            handles.retain(|handle| {
                source_for_id(handle.object().source(), sources)
                    .is_ok_and(|source| selected.binary_search(source.locator()).is_ok())
            });
            Ok(handles)
        }
        ExtractionSelection::Addresses { addresses }
        | ExtractionSelection::BundleContainer { addresses, .. }
        | ExtractionSelection::ReferenceTraversal { addresses } => {
            let mut handles = budgeted_vec(addresses.len(), "extraction selected handles", budget)?;
            for address in addresses {
                handles.push(resolve_required_handle(view, address, budget)?);
            }
            Ok(handles)
        }
    }
}

fn resolve_required_handle(
    view: &dyn WorkspaceView,
    address: &ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<RevisionedObjectHandle, ExtractionPlanError> {
    match view.resolve_object(address, budget)? {
        WorkspaceLookup::Resolved(handle) => Ok(handle),
        WorkspaceLookup::Unloaded => Err(ExtractionPlanError::ObjectUnloaded(
            clone_object_address(address, "unloaded object address", budget)?,
        )),
        WorkspaceLookup::Missing => Err(ExtractionPlanError::ObjectMissing(clone_object_address(
            address,
            "missing object address",
            budget,
        )?)),
        WorkspaceLookup::Ambiguous { candidates } => Err(ExtractionPlanError::ObjectAmbiguous {
            address: clone_object_address(address, "ambiguous object address", budget)?,
            candidates: candidates.len(),
        }),
        WorkspaceLookup::Invalid { .. } => Err(ExtractionPlanError::ObjectInvalid(
            clone_object_address(address, "invalid object address", budget)?,
        )),
    }
}

fn source_for_id(
    id: SourceId,
    sources: &[WorkspaceSource],
) -> Result<&WorkspaceSource, ExtractionPlanError> {
    sources
        .iter()
        .find(|source| source.id() == id)
        .ok_or(ExtractionPlanError::SourceMissing(id))
}

fn object_name(
    object: &WorkspaceObject,
    budget: &mut AssetLoadBudget,
) -> Result<Option<String>, ExtractionPlanError> {
    object
        .class()
        .get("m_Name")
        .or_else(|| object.class().get("name"))
        .and_then(UnityValue::as_str)
        .map(|name| clone_string(name, "extraction artifact object name", budget))
        .transpose()
}

fn allocate_path(
    prefix: Option<&ExtractionPath>,
    address: &ObjectAddress,
    source: &SourceLocator,
    class_id: i32,
    class_name: &str,
    object_name: Option<&str>,
    extension: &str,
    raw_fallback: bool,
    budget: &mut AssetLoadBudget,
) -> Result<ExtractionPath, ExtractionPlanError> {
    const SOURCE_LIMIT: usize = 48;
    const CLASS_LIMIT: usize = 48;
    const NAME_LIMIT: usize = 64;
    const DIGEST_HEX_BYTES: usize = 64;

    let digest = canonical_digest(address)
        .map_err(|error| ExtractionPlanError::CanonicalAddress(error.to_string()))?;
    let source_name = source.root_alias().as_str();
    let artifact_name = object_name.unwrap_or_else(|| {
        address
            .yaml_anchor()
            .unwrap_or(if address.kind() == ObjectKind::Binary {
                "object"
            } else {
                "document"
            })
    });
    let fallback = if raw_fallback { ".raw" } else { "" };
    let requested = [
        prefix.map_or(0, |prefix| prefix.as_str().len() + 1),
        "sources/source-".len(),
        slug_capacity_bound(source_name, SOURCE_LIMIT),
        "/class-".len(),
        11,
        "-".len(),
        slug_capacity_bound(class_name, CLASS_LIMIT),
        "/".len(),
        slug_capacity_bound(artifact_name, NAME_LIMIT),
        "--".len(),
        DIGEST_HEX_BYTES,
        fallback.len(),
        ".".len(),
        extension.len(),
    ]
    .into_iter()
    .try_fold(0_usize, |total, part| total.checked_add(part))
    .ok_or(ExtractionPlanError::ArithmeticOverflow {
        resource: "extraction relative path",
    })?;
    budget.check_bytes(usize_to_u64(requested, "extraction relative path")?)?;
    let mut relative = String::new();
    relative
        .try_reserve_exact(requested)
        .map_err(|source| ExtractionPlanError::Allocation {
            resource: "extraction relative path",
            requested,
            source,
        })?;
    if let Some(prefix) = prefix {
        relative.push_str(prefix.as_str());
        relative.push('/');
    }
    relative.push_str("sources/source-");
    push_slug(&mut relative, source_name, SOURCE_LIMIT);
    relative.push_str("/class-");
    write!(&mut relative, "{class_id}").map_err(|_| ExtractionPlanError::PathFormatting)?;
    relative.push('-');
    push_slug(&mut relative, class_name, CLASS_LIMIT);
    relative.push('/');
    push_slug(&mut relative, artifact_name, NAME_LIMIT);
    relative.push_str("--");
    push_digest_hex(&mut relative, digest);
    relative.push_str(fallback);
    relative.push('.');
    relative.push_str(extension);

    ExtractionPath::from_string_with_budget(relative, budget).map_err(map_model_error)
}

const fn slug_capacity_bound(value: &str, maximum: usize) -> usize {
    let input_bound = if value.len() < maximum {
        value.len()
    } else {
        maximum
    };
    if input_bound < "unnamed".len() {
        "unnamed".len()
    } else {
        input_bound
    }
}

fn push_slug(output: &mut String, value: &str, maximum: usize) {
    let start = output.len();
    let mut separator = false;
    for character in value.chars() {
        let mapped = if character.is_ascii_alphanumeric() {
            Some(character.to_ascii_lowercase())
        } else if matches!(character, '-' | '_') {
            Some(character)
        } else {
            None
        };
        match mapped {
            Some(character) if output.len() - start < maximum => {
                output.push(character);
                separator = false;
            }
            Some(_) => break,
            None if !separator && output.len() != start && output.len() - start < maximum => {
                output.push('_');
                separator = true;
            }
            None => {}
        }
    }
    while output.len() != start && (output.ends_with('_') || output.ends_with('-')) {
        output.pop();
    }
    if output.len() == start {
        output.push_str("unnamed");
    }
}

fn push_digest_hex(output: &mut String, digest: unity_asset_core::DigestV1) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &byte in digest.as_bytes() {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
}

#[cfg(feature = "decode")]
fn resolve_extraction_stream(
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
    let resource = match resolver.resolve(owner, stream_path, offset, size, budget)? {
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
    let expectation = SourceExpectationOwned::from_streamed_resource(&resource, budget)?;
    let range = ExtractionSourceRange::new(
        clone_source_locator(
            resource.source().locator(),
            "decoded stream source range locator",
            budget,
        )?,
        offset,
        size,
    )
    .map_err(map_model_error)?;
    Ok(ResolvedExtractionStream { range, expectation })
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

#[derive(Debug, Error)]
pub enum ExtractionPlanError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Reference(#[from] ReferenceGraphError),
    #[error(transparent)]
    ContainerContract(#[from] super::container::BundleContainerContractError),
    #[error(transparent)]
    Diagnostic(#[from] unity_asset_core::DiagnosticError),
    #[error(transparent)]
    FieldPath(#[from] unity_asset_core::FieldPathError),
    #[error("failed to reserve {requested} capacity units for {resource}: {source}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        #[source]
        source: std::collections::TryReserveError,
    },
    #[error("extraction model rejected the plan: {0}")]
    Model(String),
    #[error("reference graph violated an extraction invariant: {0}")]
    ReferenceInvariant(&'static str),
    #[error("reference graph does not describe the extraction workspace revision")]
    ReferenceContextMismatch,
    #[error("bundle-container selection requires a caller-supplied ReferenceGraph")]
    ReferenceGraphRequired,
    #[error("an incomplete reference graph cannot drive bundle-container extraction")]
    IncompleteReferenceGraph,
    #[error("an incomplete reference traversal cannot be used as an extraction selection")]
    IncompleteReferenceTraversal,
    #[error("required decoded representation is unavailable for {address:?}: {reason:?}")]
    RequiredDecodedUnavailable {
        address: ObjectAddress,
        reason: ExtractionDiagnosticCode,
    },
    #[error("object is not loaded: {0:?}")]
    ObjectUnloaded(ObjectAddress),
    #[error("object does not exist: {0:?}")]
    ObjectMissing(ObjectAddress),
    #[error("object address {address:?} is ambiguous across {candidates} candidates")]
    ObjectAmbiguous {
        address: ObjectAddress,
        candidates: usize,
    },
    #[error("object address is invalid: {0:?}")]
    ObjectInvalid(ObjectAddress),
    #[error("workspace source is missing: {0:?}")]
    SourceMissing(SourceId),
    #[error("source {locator:?} has conflicting fingerprints {first} and {second}")]
    SourceFingerprintConflict {
        locator: SourceLocator,
        first: SourceFingerprint,
        second: SourceFingerprint,
    },
    #[error("stream source is missing: {0:?}")]
    StreamSourceMissing(SourceLocator),
    #[error("object identity cannot be represented as an ObjectAddress")]
    InvalidObjectIdentity,
    #[error("invalid streamed resource path: {0:?}")]
    InvalidStreamPath(String),
    #[error("streamed resource {stream_path:?} is missing for {owner:?}")]
    MissingStreamResource {
        owner: SourceLocator,
        stream_path: String,
    },
    #[error("streamed resource {stream_path:?} is ambiguous for {owner:?}")]
    AmbiguousStreamResource {
        owner: SourceLocator,
        stream_path: String,
    },
    #[error("failed to encode canonical object address: {0}")]
    CanonicalAddress(String),
    #[error("failed to format extraction output path")]
    PathFormatting,
    #[error("failed to measure canonical YAML output: {0}")]
    YamlSizing(String),
    #[error("arithmetic overflow while planning {resource}")]
    ArithmeticOverflow { resource: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colliding_media_slugs_keep_stable_identity_paths() {
        let source = SourceLocator::path("media.assets").unwrap();
        let texture = ObjectAddress::binary_direct(source.clone(), 41).unwrap();
        let sprite = ObjectAddress::binary_direct(source.clone(), 42).unwrap();
        let mut budget = AssetLoadBudget::default();

        let texture_path = allocate_path(
            None,
            &texture,
            &source,
            class_ids::TEXTURE_2D,
            "Texture2D",
            Some("UI/Hero Icon"),
            "png",
            false,
            &mut budget,
        )
        .unwrap();
        let sprite_path = allocate_path(
            None,
            &sprite,
            &source,
            class_ids::SPRITE,
            "Sprite",
            Some("UI?Hero Icon"),
            "png",
            false,
            &mut budget,
        )
        .unwrap();

        assert_ne!(texture_path, sprite_path);
        assert!(texture_path.as_str().contains("/ui_hero_icon--"));
        assert!(sprite_path.as_str().contains("/ui_hero_icon--"));

        let repeated = allocate_path(
            None,
            &texture,
            &source,
            class_ids::TEXTURE_2D,
            "Texture2D",
            Some("UI/Hero Icon"),
            "png",
            false,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(texture_path, repeated);
    }

    #[test]
    fn allocated_path_charges_every_retained_name_byte() {
        let source = SourceLocator::path("media.assets").unwrap();
        let address = ObjectAddress::binary_direct(source.clone(), 41).unwrap();
        let mut budget = AssetLoadBudget::default();

        let path = allocate_path(
            None,
            &address,
            &source,
            class_ids::TEXTURE_2D,
            "Texture2D",
            Some("UI/Hero Icon"),
            "png",
            false,
            &mut budget,
        )
        .unwrap();

        assert_eq!(budget.usage().bytes, path.retained_bytes().unwrap());
    }

    #[test]
    fn unavailable_decoding_preserves_the_machine_actionable_reason() {
        let address =
            ObjectAddress::binary_direct(SourceLocator::path("media.assets").unwrap(), 41).unwrap();
        let raw = ContentChoice::decoded(
            ExtractionArtifactKind::BinaryRaw,
            "bin",
            PlannedContent::RawBinary,
            1,
            None,
        );

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
            ContentChoice::decoded(
                ExtractionArtifactKind::BinaryRaw,
                "bin",
                PlannedContent::RawBinary,
                1,
                None,
            ),
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
    fn decoded_resource_failures_keep_missing_and_unresolved_categories_distinct() {
        let owner = SourceLocator::path("media.assets").unwrap();
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
        let resource_source = SourceId::new(
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
        assert_eq!(
            decoded_resource_failure(&invalid),
            Some(ExtractionDiagnosticCode::MissingResource)
        );
        assert_eq!(
            decoded_resource_failure(&owner_missing),
            Some(ExtractionDiagnosticCode::MissingResource)
        );
        assert_eq!(
            decoded_resource_failure(&out_of_bounds),
            Some(ExtractionDiagnosticCode::MissingResource)
        );
        assert_eq!(
            decoded_resource_failure(&ambiguous_stream),
            Some(ExtractionDiagnosticCode::UnresolvedDependency)
        );
        assert_eq!(
            decoded_resource_failure(&unresolved),
            Some(ExtractionDiagnosticCode::UnresolvedDependency)
        );
    }

    #[cfg(feature = "decode")]
    #[test]
    fn decoded_batch_builds_the_streamed_resource_index_once() {
        let sample = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/samples/char_118_yuki.ab");
        let mut workspace = crate::workspace::AssetWorkspace::new().unwrap();
        workspace
            .load_path(&sample, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        StreamedResourceResolver::reset_test_build_count();

        let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
            .with_filter(
                super::super::model::ExtractionFilter::new(
                    [class_ids::AUDIO_CLIP],
                    None,
                    None,
                    None,
                )
                .unwrap(),
            );
        let plan = ExtractionPlanner::new(&snapshot)
            .plan(request, &mut AssetLoadBudget::default())
            .unwrap();

        assert!(plan.artifacts().len() > 1, "fixture must exercise a batch");
        assert_eq!(StreamedResourceResolver::test_build_count(), 1);
    }

    #[cfg(not(feature = "decode"))]
    #[test]
    fn unavailable_decode_does_not_build_a_streamed_resource_index() {
        let sample = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/samples/banner_1");
        let mut workspace = crate::workspace::AssetWorkspace::new().unwrap();
        workspace
            .load_path(&sample, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        StreamedResourceResolver::reset_test_build_count();

        let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded);
        ExtractionPlanner::new(&snapshot)
            .plan(request, &mut AssetLoadBudget::default())
            .expect_err("decode must remain unavailable without the feature");

        assert_eq!(StreamedResourceResolver::test_build_count(), 0);
    }
}
