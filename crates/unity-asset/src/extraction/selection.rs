use std::collections::BTreeMap;
use std::io::{self, Write};
#[cfg(feature = "decode")]
use std::mem::size_of;

use thiserror::Error;
use unity_asset_binary::asset::class_ids;
use unity_asset_binary::object::UnityObject;
#[cfg(feature = "decode")]
use unity_asset_core::SourceKind;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ContractError, DigestV1, ObjectAddress, ObjectKind,
    RevisionedObjectHandle, SourceId, SourceLocator, UnityValue,
};
use unity_asset_yaml::UnityYamlSerializer;

#[cfg(feature = "decode")]
use unity_asset_decode::{
    audio::{AudioClipConverter, AudioCompressionFormat, MAX_VORBIS_SETUP_PACKET_BYTES},
    sprite::SpriteProcessor,
    texture::TextureProcessor,
};

#[cfg(feature = "decode")]
use super::model::ExtractionSourceRange;
use super::model::{
    ExtractionArtifactKind, ExtractionDiagnostic, ExtractionDiagnosticCode, ExtractionPath,
    ExtractionPlan, ExtractionRepresentationPolicy, ExtractionRequest, ExtractionSelection,
    ExtractionSourceExpectation, PlannedArtifact, PlannedContent,
};
use super::source_budget_error;
use crate::reference::{
    RawReferenceTarget, ReferenceGraph, ReferenceGraphError, ReferenceResolution,
    ReferenceTraversal,
};
use crate::workspace::{
    WorkspaceError, WorkspaceLookup, WorkspaceObject, WorkspaceObjectValue, WorkspaceSource,
    WorkspaceView,
};

/// Plans deterministic extraction artifacts against one immutable workspace view.
pub struct ExtractionPlanner<'view> {
    view: &'view dyn WorkspaceView,
    references: Option<&'view ReferenceGraph>,
}

#[derive(Default)]
struct StreamSourceIndex<'source> {
    #[cfg_attr(not(feature = "decode"), allow(dead_code))]
    by_basename: BTreeMap<String, Vec<&'source WorkspaceSource>>,
}

impl<'source> StreamSourceIndex<'source> {
    #[cfg(feature = "decode")]
    fn new(
        sources: &'source [WorkspaceSource],
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ExtractionPlanError> {
        let mut index = Self::default();
        for source in sources
            .iter()
            .filter(|source| source.kind() == SourceKind::StreamedResource)
        {
            let candidate_name = source
                .locator()
                .members()
                .last()
                .map(|step| step.name())
                .unwrap_or_else(|| source.locator().root_alias().as_str());
            let key_bytes = u64::try_from(candidate_name.len()).map_err(|_| {
                BudgetError::ArithmeticOverflow {
                    resource: "stream source index key",
                }
            })?;
            let reference_bytes = u64::try_from(size_of::<&WorkspaceSource>()).map_err(|_| {
                BudgetError::ArithmeticOverflow {
                    resource: "stream source index entry",
                }
            })?;
            let allocation_bytes =
                key_bytes
                    .checked_add(reference_bytes)
                    .ok_or(BudgetError::ArithmeticOverflow {
                        resource: "stream source index entry",
                    })?;
            budget.check_entries(1)?;
            budget.check_bytes(allocation_bytes)?;
            let key = candidate_name.to_ascii_lowercase();
            budget.consume_entries(1)?;
            budget.consume_bytes(allocation_bytes)?;
            index.by_basename.entry(key).or_default().push(source);
        }
        Ok(index)
    }

    #[cfg(not(feature = "decode"))]
    fn new(
        _sources: &'source [WorkspaceSource],
        _budget: &mut AssetLoadBudget,
    ) -> Result<Self, ExtractionPlanError> {
        Ok(Self::default())
    }

    #[cfg(feature = "decode")]
    fn candidates(
        &self,
        stream_path: &str,
        budget: &mut AssetLoadBudget,
    ) -> Result<Option<&[&'source WorkspaceSource]>, ExtractionPlanError> {
        let basename = stream_basename(stream_path)
            .ok_or_else(|| ExtractionPlanError::InvalidStreamPath(stream_path.to_owned()))?;
        let bytes = u64::try_from(basename.len()).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "stream source lookup key",
        })?;
        budget.check_bytes(bytes)?;
        let key = basename.to_ascii_lowercase();
        budget.consume_bytes(bytes)?;
        Ok(self.by_basename.get(key.as_str()).map(Vec::as_slice))
    }
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

        let mut sources = self.view.sources(budget)?;
        sources.sort_by(|left, right| left.locator().cmp(right.locator()));
        let stream_sources = if request.representation() == ExtractionRepresentationPolicy::RawOnly
        {
            StreamSourceIndex::default()
        } else {
            StreamSourceIndex::new(&sources, budget)?
        };
        let handles = selected_handles(self.view, &request, &sources, budget)?;
        let mut candidates = Vec::new();
        for handle in handles {
            budget.consume_entries(1)?;
            let address = address_for_handle(self.view, &handle, &sources, budget)?;
            candidates.push((address, handle));
        }
        candidates.sort_by(|left, right| left.0.cmp(&right.0));
        candidates.dedup_by(|left, right| left.0 == right.0);

        let mut source_expectations = BTreeMap::<SourceLocator, _>::new();
        let mut artifacts = Vec::new();
        for (address, handle) in candidates {
            let object = self.view.read_object(&handle, budget)?;
            if !request
                .filter()
                .matches_class(object.class().class_id, &object.class().class_name)
            {
                continue;
            }
            let object_name = object_name(&object);
            if !request.filter().matches_object_name(object_name.as_deref()) {
                continue;
            }
            if request.filter().limit().is_some_and(|limit| {
                u64::try_from(artifacts.len()).is_ok_and(|count| count >= limit)
            }) {
                break;
            }

            let owner = source_for_id(handle.object().source(), &sources)?;
            source_expectations.insert(owner.locator().clone(), owner.fingerprint());
            let choice = self.plan_content(
                &address,
                &object,
                owner,
                &sources,
                &stream_sources,
                request.representation(),
                budget,
            )?;
            for expectation in choice.source_expectations {
                source_expectations.insert(expectation.locator, expectation.fingerprint);
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
                object.class().class_id,
                &object.class().class_name,
                object_name.as_deref(),
                &choice.preferred_extension,
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
                        object.class().class_id,
                        &object.class().class_name,
                        object_name.as_deref(),
                        "bin",
                        true,
                        budget,
                    )?,
                    content,
                )),
                None => None,
            };
            artifacts.push(
                PlannedArtifact::new(
                    ordinal,
                    address,
                    object.class().class_id,
                    object.class().class_name.clone(),
                    object_name,
                    choice.preferred_kind,
                    preferred_path,
                    choice.preferred_content,
                    fallback,
                    choice.working_set_bytes,
                    choice.diagnostics,
                )
                .map_err(|error| ExtractionPlanError::Model(error.to_string()))?,
            );
        }

        let sources = source_expectations
            .into_iter()
            .map(|(locator, fingerprint)| ExtractionSourceExpectation::new(locator, fingerprint))
            .collect();
        ExtractionPlan::new(
            self.view.workspace_id(),
            self.view.revision(),
            request,
            sources,
            artifacts,
        )
        .map_err(|error| ExtractionPlanError::Model(error.to_string()))
    }

    pub fn plan_handles(
        &self,
        handles: &[RevisionedObjectHandle],
        representation: ExtractionRepresentationPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        let sources = self.view.sources(budget)?;
        let mut addresses = Vec::new();
        for handle in handles {
            handle.validate_context(self.view.workspace_id(), self.view.revision())?;
            addresses.push(address_for_handle(self.view, handle, &sources, budget)?);
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
        self.plan_handles(
            &traversal.nodes().cloned().collect::<Vec<_>>(),
            representation,
            budget,
        )
    }

    pub fn plan_bundle_containers(
        &self,
        pattern: &str,
        representation: ExtractionRepresentationPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        let addresses = self.bundle_container_addresses(pattern, budget)?;
        let request =
            ExtractionRequest::bundle_container(pattern.to_owned(), addresses, representation)
                .map_err(|error| ExtractionPlanError::Model(error.to_string()))?;
        self.plan(request, budget)
    }

    /// Resolves `AssetBundle.m_Container` entries into stable object addresses.
    ///
    /// Callers that need filters or a path prefix can build an [`ExtractionRequest`] from the
    /// returned addresses, then pass it to [`Self::plan`].
    pub fn bundle_container_addresses(
        &self,
        pattern: &str,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<ObjectAddress>, ExtractionPlanError> {
        self.validate_reference_context()?;
        let references = self
            .references
            .ok_or(ExtractionPlanError::ReferenceGraphRequired)?;
        let mut addresses = Vec::new();
        let mut handles = self.view.objects(budget)?;
        handles.sort_by(|left, right| left.object().cmp(right.object()));
        for handle in handles {
            let object = self.view.read_object(&handle, budget)?;
            let WorkspaceObjectValue::Binary(binary) = object.value() else {
                continue;
            };
            if binary.class_id() != class_ids::ASSET_BUNDLE {
                continue;
            }
            for (asset_path, file_id, path_id) in container_entries(binary) {
                if !asset_path_matches(pattern, asset_path) || path_id == 0 {
                    continue;
                }
                if let Some(address) =
                    resolve_reference_address(references, &handle, file_id, path_id)?
                {
                    addresses.push(address);
                }
            }
        }
        addresses.sort_unstable();
        addresses.dedup();
        Ok(addresses)
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
        _stream_sources: &StreamSourceIndex<'_>,
        policy: ExtractionRepresentationPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<ContentChoice, ExtractionPlanError> {
        #[cfg(feature = "decode")]
        let (owner, sources, stream_sources) = (_owner, _sources, _stream_sources);
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
            unavailable_choice(address, policy, raw)
        }

        #[cfg(feature = "decode")]
        {
            let Some(version) = object
                .schema_provenance()
                .binary_version()
                .and_then(|version| version.unity())
                .cloned()
            else {
                return unavailable_choice(address, policy, raw);
            };

            match binary.class_id() {
                class_ids::AUDIO_CLIP => {
                    let converter = AudioClipConverter::new(version.clone());
                    let clip = match converter.from_unity_object(binary) {
                        Ok(clip) => clip,
                        Err(_) => return unavailable_choice(address, policy, raw),
                    };
                    let stream = if clip.data.is_empty() && clip.is_streamed() {
                        match resolve_stream_range(
                            self.view,
                            owner,
                            stream_sources,
                            &clip.stream_info.path,
                            clip.stream_info.offset,
                            u64::from(clip.stream_info.size),
                            budget,
                        ) {
                            Ok(range) => Some(range),
                            Err(error) => {
                                let Some(code) = decoded_resource_failure(&error) else {
                                    return Err(error);
                                };
                                return unavailable_choice_with(address, policy, raw, code);
                            }
                        }
                    } else {
                        None
                    };
                    let extension = clip.compression_format().extension().to_owned();
                    let encoded_audio_bytes = match stream.as_ref() {
                        Some(stream) => stream.size(),
                        None => usize_to_u64(clip.data.len(), "embedded audio size")?,
                    };
                    let output_bound =
                        if clip.compression_format() == AudioCompressionFormat::Vorbis {
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
                    let expectations = stream
                        .as_ref()
                        .map(|range| source_expectation_for_locator(range.source(), sources))
                        .transpose()?
                        .into_iter()
                        .collect();
                    Ok(ContentChoice::decoded_with_sources(
                        ExtractionArtifactKind::Audio,
                        extension.clone(),
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
                    let processor = TextureProcessor::new(version.clone());
                    let texture = match processor.convert_object(binary) {
                        Ok(texture) => texture,
                        Err(_) => return unavailable_choice(address, policy, raw),
                    };
                    let stream = if texture.image_data.is_empty() && texture.is_streamed() {
                        match resolve_stream_range(
                            self.view,
                            owner,
                            stream_sources,
                            &texture.stream_info.path,
                            texture.stream_info.offset,
                            u64::from(texture.stream_info.size),
                            budget,
                        ) {
                            Ok(range) => Some(range),
                            Err(error) => {
                                let Some(code) = decoded_resource_failure(&error) else {
                                    return Err(error);
                                };
                                return unavailable_choice_with(address, policy, raw, code);
                            }
                        }
                    } else {
                        None
                    };
                    let image_bytes = u64::try_from(texture.width.max(0))
                        .ok()
                        .and_then(|width| {
                            u64::try_from(texture.height.max(0))
                                .ok()
                                .and_then(|height| width.checked_mul(height))
                        })
                        .and_then(|pixels| pixels.checked_mul(4))
                        .ok_or(ExtractionPlanError::ArithmeticOverflow {
                            resource: "texture working set",
                        })?;
                    let expectations = stream
                        .as_ref()
                        .map(|range| source_expectation_for_locator(range.source(), sources))
                        .transpose()?
                        .into_iter()
                        .collect();
                    let stream_bytes = stream.as_ref().map_or(0, ExtractionSourceRange::size);
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
                    stream_sources,
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
        stream_sources: &StreamSourceIndex<'_>,
        version: unity_asset_binary::unity_version::UnityVersion,
        policy: ExtractionRepresentationPolicy,
        raw: ContentChoice,
        budget: &mut AssetLoadBudget,
    ) -> Result<ContentChoice, ExtractionPlanError> {
        let processor = SpriteProcessor::new(version.clone());
        let parsed = match processor.parse_sprite(binary) {
            Ok(parsed) => parsed.sprite,
            Err(_) => return unavailable_choice(address, policy, raw),
        };
        let Some((file_id, path_id)) = sprite_texture_pptr(binary).or_else(|| {
            (parsed.render_data.texture_path_id != 0)
                .then_some((0, parsed.render_data.texture_path_id))
        }) else {
            return unavailable_choice_with(
                address,
                policy,
                raw,
                ExtractionDiagnosticCode::UnresolvedDependency,
            );
        };
        let texture_address = if file_id == 0 {
            ObjectAddress::binary_at(owner.locator().clone(), path_id)?
        } else {
            let Some(references) = self.references else {
                return unavailable_choice_with(
                    address,
                    policy,
                    raw,
                    ExtractionDiagnosticCode::UnresolvedDependency,
                );
            };
            let owner_handle = resolve_required_handle(self.view, address, budget)?;
            let Some(texture) =
                resolve_reference_address(references, &owner_handle, file_id, path_id)?
            else {
                return unavailable_choice_with(
                    address,
                    policy,
                    raw,
                    ExtractionDiagnosticCode::UnresolvedDependency,
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
                return unavailable_choice_with(address, policy, raw, code);
            }
        };
        let texture_object = self.view.read_object(&texture_handle, budget)?;
        let WorkspaceObjectValue::Binary(texture_binary) = texture_object.value() else {
            return unavailable_choice_with(
                address,
                policy,
                raw,
                ExtractionDiagnosticCode::UnresolvedDependency,
            );
        };
        if texture_binary.class_id() != class_ids::TEXTURE_2D {
            return unavailable_choice_with(
                address,
                policy,
                raw,
                ExtractionDiagnosticCode::UnresolvedDependency,
            );
        }
        let texture_owner = match source_for_id(texture_handle.object().source(), sources) {
            Ok(owner) => owner,
            Err(error) => {
                let Some(code) = decoded_resource_failure(&error) else {
                    return Err(error);
                };
                return unavailable_choice_with(address, policy, raw, code);
            }
        };
        let texture_processor = TextureProcessor::new(version.clone());
        let texture = match texture_processor.convert_object(texture_binary) {
            Ok(texture) => texture,
            Err(_) => return unavailable_choice(address, policy, raw),
        };
        let texture_stream = if texture.image_data.is_empty() && texture.is_streamed() {
            match resolve_stream_range(
                self.view,
                texture_owner,
                stream_sources,
                &texture.stream_info.path,
                texture.stream_info.offset,
                u64::from(texture.stream_info.size),
                budget,
            ) {
                Ok(range) => Some(range),
                Err(error) => {
                    let Some(code) = decoded_resource_failure(&error) else {
                        return Err(error);
                    };
                    return unavailable_choice_with(address, policy, raw, code);
                }
            }
        } else {
            None
        };
        let mut expectations = vec![SourceExpectationOwned::from_source(texture_owner)];
        if let Some(range) = texture_stream.as_ref() {
            expectations.push(source_expectation_for_locator(range.source(), sources)?);
        }
        let texture_stream_bytes = texture_stream
            .as_ref()
            .map_or(0, ExtractionSourceRange::size);
        let image_bytes = u64::try_from(texture.width.max(0))
            .ok()
            .and_then(|width| {
                u64::try_from(texture.height.max(0))
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(ExtractionPlanError::ArithmeticOverflow {
                resource: "sprite working set",
            })?;
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
    preferred_extension: String,
    preferred_content: PlannedContent,
    fallback: Option<(ExtractionArtifactKind, PlannedContent)>,
    working_set_bytes: u64,
    diagnostics: Vec<ExtractionDiagnostic>,
    source_expectations: Vec<SourceExpectationOwned>,
}

impl ContentChoice {
    fn decoded(
        kind: ExtractionArtifactKind,
        extension: impl Into<String>,
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
        extension: impl Into<String>,
        content: PlannedContent,
        working_set_bytes: u64,
        fallback: Option<(ExtractionArtifactKind, PlannedContent)>,
        source_expectations: Vec<SourceExpectationOwned>,
    ) -> Self {
        Self {
            preferred_kind: kind,
            preferred_extension: extension.into(),
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
    #[cfg(feature = "decode")]
    fn from_source(source: &WorkspaceSource) -> Self {
        Self {
            locator: source.locator().clone(),
            fingerprint: source.fingerprint(),
        }
    }
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
) -> Result<ContentChoice, ExtractionPlanError> {
    unavailable_choice_with(
        address,
        policy,
        raw,
        ExtractionDiagnosticCode::DecodedUnavailable,
    )
}

fn unavailable_choice_with(
    address: &ObjectAddress,
    policy: ExtractionRepresentationPolicy,
    mut raw: ContentChoice,
    code: ExtractionDiagnosticCode,
) -> Result<ContentChoice, ExtractionPlanError> {
    if policy == ExtractionRepresentationPolicy::RequireDecoded {
        return Err(required_or_unavailable(address, code));
    }
    raw.diagnostics
        .push(ExtractionDiagnostic::new(code, Some(address.clone())));
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

fn required_or_unavailable(
    address: &ObjectAddress,
    reason: ExtractionDiagnosticCode,
) -> ExtractionPlanError {
    ExtractionPlanError::RequiredDecodedUnavailable {
        address: address.clone(),
        reason,
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
        | ExtractionSelection::ReferenceTraversal { addresses } => addresses
            .iter()
            .map(|address| resolve_required_handle(view, address, budget))
            .collect(),
    }
}

fn resolve_required_handle(
    view: &dyn WorkspaceView,
    address: &ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<RevisionedObjectHandle, ExtractionPlanError> {
    match view.resolve_object(address, budget)? {
        WorkspaceLookup::Resolved(handle) => Ok(handle),
        WorkspaceLookup::Unloaded => Err(ExtractionPlanError::ObjectUnloaded(address.clone())),
        WorkspaceLookup::Missing => Err(ExtractionPlanError::ObjectMissing(address.clone())),
        WorkspaceLookup::Ambiguous { candidates } => Err(ExtractionPlanError::ObjectAmbiguous {
            address: address.clone(),
            candidates: candidates.len(),
        }),
        WorkspaceLookup::Invalid { .. } => Err(ExtractionPlanError::ObjectInvalid(address.clone())),
    }
}

fn address_for_handle(
    view: &dyn WorkspaceView,
    handle: &RevisionedObjectHandle,
    sources: &[WorkspaceSource],
    _: &mut AssetLoadBudget,
) -> Result<ObjectAddress, ExtractionPlanError> {
    handle.validate_context(view.workspace_id(), view.revision())?;
    let source = source_for_id(handle.object().source(), sources)?;
    match handle.object().kind() {
        ObjectKind::Binary => ObjectAddress::binary_at(
            source.locator().clone(),
            handle
                .object()
                .binary_path_id()
                .ok_or(ExtractionPlanError::InvalidObjectIdentity)?,
        )
        .map_err(Into::into),
        ObjectKind::Yaml => ObjectAddress::yaml_with_selector(
            source.locator().clone(),
            if let Some(anchor) = handle.object().yaml_anchor() {
                unity_asset_core::YamlDocumentSelector::anchor(anchor.to_owned())?
            } else {
                unity_asset_core::YamlDocumentSelector::ordinal(
                    handle
                        .object()
                        .yaml_document_ordinal()
                        .ok_or(ExtractionPlanError::InvalidObjectIdentity)?,
                )
            },
        )
        .map_err(Into::into),
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

#[cfg(feature = "decode")]
fn source_expectation_for_locator(
    locator: &SourceLocator,
    sources: &[WorkspaceSource],
) -> Result<SourceExpectationOwned, ExtractionPlanError> {
    sources
        .iter()
        .find(|source| source.locator() == locator)
        .map(SourceExpectationOwned::from_source)
        .ok_or_else(|| ExtractionPlanError::StreamSourceMissing(locator.clone()))
}

fn object_name(object: &WorkspaceObject) -> Option<String> {
    match object.value() {
        WorkspaceObjectValue::Binary(object) => object.name(),
        WorkspaceObjectValue::Yaml(object) => object
            .class()
            .get("m_Name")
            .or_else(|| object.class().get("name"))
            .and_then(UnityValue::as_str)
            .map(str::to_owned),
    }
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
    let encoded = serde_json::to_vec(address)
        .map_err(|error| ExtractionPlanError::CanonicalAddress(error.to_string()))?;
    let digest = DigestV1::hash_bytes(&encoded).to_string();
    let identity = digest
        .strip_prefix("blake3-v1:")
        .ok_or_else(|| ExtractionPlanError::CanonicalAddress("invalid DigestV1 display".into()))?;
    let source = slug(source.root_alias().as_str(), 48);
    let class = slug(class_name, 48);
    let name = slug(
        object_name.unwrap_or_else(|| {
            address
                .yaml_anchor()
                .unwrap_or(if address.kind() == ObjectKind::Binary {
                    "object"
                } else {
                    "document"
                })
        }),
        64,
    );
    let fallback = if raw_fallback { ".raw" } else { "" };
    let relative = format!(
        "sources/source-{source}/class-{class_id}-{class}/{name}--{identity}{fallback}.{extension}"
    );
    let relative = match prefix {
        Some(prefix) => format!("{}/{relative}", prefix.as_str()),
        None => relative,
    };
    budget.consume_bytes(u64::try_from(relative.len()).map_err(|_| {
        ExtractionPlanError::ArithmeticOverflow {
            resource: "extraction relative path",
        }
    })?)?;
    ExtractionPath::new(relative).map_err(|error| ExtractionPlanError::Model(error.to_string()))
}

fn slug(value: &str, maximum: usize) -> String {
    let mut output = String::with_capacity(value.len().min(maximum));
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
            Some(character) if output.len() < maximum => {
                output.push(character);
                separator = false;
            }
            Some(_) => break,
            None if !separator && !output.is_empty() && output.len() < maximum => {
                output.push('_');
                separator = true;
            }
            None => {}
        }
    }
    while output.ends_with('_') || output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        output.push_str("unnamed");
    }
    output
}

#[cfg(feature = "decode")]
fn resolve_stream_range(
    view: &dyn WorkspaceView,
    owner: &WorkspaceSource,
    stream_sources: &StreamSourceIndex<'_>,
    stream_path: &str,
    offset: u64,
    size: u64,
    budget: &mut AssetLoadBudget,
) -> Result<ExtractionSourceRange, ExtractionPlanError> {
    let Some(candidates) = stream_sources.candidates(stream_path, budget)? else {
        return Err(ExtractionPlanError::MissingStreamResource {
            owner: owner.locator().clone(),
            stream_path: stream_path.to_owned(),
        });
    };
    let mut selected = None;
    let mut ambiguous = false;
    for candidate in candidates {
        let candidate = *candidate;
        let score = stream_source_score(owner, candidate);
        match selected {
            None => selected = Some((score, candidate)),
            Some((best_score, _)) if score < best_score => {
                selected = Some((score, candidate));
                ambiguous = false;
            }
            Some((best_score, _)) if score == best_score => ambiguous = true,
            Some(_) => {}
        }
    }
    let Some((_, source)) = selected else {
        return Err(ExtractionPlanError::MissingStreamResource {
            owner: owner.locator().clone(),
            stream_path: stream_path.to_owned(),
        });
    };
    if ambiguous {
        return Err(ExtractionPlanError::AmbiguousStreamResource {
            owner: owner.locator().clone(),
            stream_path: stream_path.to_owned(),
        });
    }
    let _ = view.read_source_range(source.id(), offset, size, budget)?;
    ExtractionSourceRange::new(source.locator().clone(), offset, size)
        .map_err(|error| ExtractionPlanError::Model(error.to_string()))
}

#[cfg(feature = "decode")]
fn stream_source_score(owner: &WorkspaceSource, candidate: &WorkspaceSource) -> u8 {
    if owner.parent().is_some() && owner.parent() == candidate.parent() {
        0
    } else if owner.locator().root_alias() == candidate.locator().root_alias() {
        1
    } else {
        2
    }
}

#[cfg(feature = "decode")]
fn stream_basename(path: &str) -> Option<&str> {
    path.rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
}

fn container_entries(object: &UnityObject) -> Vec<(&str, i32, i64)> {
    let Some(UnityValue::Array(items)) = object.get("m_Container") else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item {
            UnityValue::Array(pair) if pair.len() == 2 => {
                let asset_path = pair[0].as_str()?;
                let (file_id, path_id) = scan_pptr(&pair[1])?;
                Some((asset_path, file_id, path_id))
            }
            UnityValue::Object(pair) => {
                let asset_path = pair.get("first")?.as_str()?;
                let target = pair.get("second").or_else(|| pair.get("value"))?;
                let (file_id, path_id) = scan_pptr(target)?;
                Some((asset_path, file_id, path_id))
            }
            _ => None,
        })
        .collect()
}

fn scan_pptr(value: &UnityValue) -> Option<(i32, i64)> {
    match value {
        UnityValue::Object(object) => {
            let file_id = object
                .get("fileID")
                .or_else(|| object.get("m_FileID"))
                .and_then(UnityValue::as_i64)
                .and_then(|value| i32::try_from(value).ok());
            let path_id = object
                .get("pathID")
                .or_else(|| object.get("m_PathID"))
                .and_then(UnityValue::as_i64);
            match (file_id, path_id) {
                (Some(file_id), Some(path_id)) => Some((file_id, path_id)),
                _ => object.values().find_map(scan_pptr),
            }
        }
        UnityValue::Array(values) => values.iter().find_map(scan_pptr),
        _ => None,
    }
}

fn resolve_reference_address(
    graph: &ReferenceGraph,
    owner: &RevisionedObjectHandle,
    file_id: i32,
    path_id: i64,
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
            return Ok(Some(graph.address(target)?.clone()));
        }
    }
    Ok(None)
}

fn asset_path_matches(pattern: &str, asset_path: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }
    let pattern = pattern.to_ascii_lowercase();
    let asset_path = asset_path.to_ascii_lowercase();
    if !pattern.contains('*') && !pattern.contains('?') {
        return asset_path.contains(&pattern);
    }
    glob_matches(pattern.as_bytes(), asset_path.as_bytes())
}

fn glob_matches(pattern: &[u8], value: &[u8]) -> bool {
    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut star, mut star_value) = (None, 0);
    while value_index < value.len() {
        if pattern.get(pattern_index) == Some(&b'?')
            || pattern.get(pattern_index) == value.get(value_index)
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern.get(pattern_index) == Some(&b'*') {
            star = Some(pattern_index);
            pattern_index += 1;
            star_value = value_index;
        } else if let Some(star_index) = star {
            star_value += 1;
            value_index = star_value;
            pattern_index = star_index + 1;
        } else {
            return false;
        }
    }
    pattern[pattern_index..].iter().all(|byte| *byte == b'*')
}

#[cfg(feature = "decode")]
fn sprite_texture_pptr(object: &UnityObject) -> Option<(i32, i64)> {
    let UnityValue::Object(render_data) = object.get("m_RD")? else {
        return None;
    };
    let UnityValue::Object(texture) = render_data.get("texture")? else {
        return None;
    };
    Some((
        i32::try_from(texture.get("m_FileID").and_then(UnityValue::as_i64)?).ok()?,
        texture.get("m_PathID").and_then(UnityValue::as_i64)?,
    ))
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
    #[error("extraction model rejected the plan: {0}")]
    Model(String),
    #[error("reference graph does not describe the extraction workspace revision")]
    ReferenceContextMismatch,
    #[error("bundle-container selection requires a caller-supplied ReferenceGraph")]
    ReferenceGraphRequired,
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

        let fallback = unavailable_choice_with(
            &address,
            ExtractionRepresentationPolicy::PreferDecoded,
            raw,
            ExtractionDiagnosticCode::MissingResource,
        )
        .unwrap();
        assert_eq!(
            fallback.diagnostics[0].code(),
            ExtractionDiagnosticCode::MissingResource
        );

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
            decoded_resource_failure(&unresolved),
            Some(ExtractionDiagnosticCode::UnresolvedDependency)
        );
    }
}
