use std::fmt;
use std::io::{Read, Seek, SeekFrom, Write};
use std::iter::FusedIterator;
use std::ops::Range;
use std::slice;
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;
use unity_asset_binary::BinaryError;
#[cfg(test)]
use unity_asset_core::vec_allocation_bytes;
use unity_asset_core::{AssetLoadBudget, DigestBuildError, DigestV1, SourceId};

use super::budget::{ArtifactBudgetTransaction, CodecScratchBudget, ScratchAllocation};
use super::format::{ExpectedArtifactFormat, StreamedResourceLayoutProof};
use super::image::{SealedImage, ValidatedImage};
use super::name::validate_unique_names;
use super::payload::GeneratedChunkWriter;
use super::{
    ArtifactBudget, ArtifactBudgetError, ArtifactBuildCounters, ArtifactFootprint,
    ArtifactNameError, ArtifactPayload, ArtifactPayloadError, ArtifactPayloadProvenance,
    ArtifactReader, ArtifactSetFootprint, ArtifactSourceDependency, ArtifactStreamError,
    ArtifactStreamReceipt, ImageBuilder, LogicalArtifactName, PreparedArtifactFormat,
    StreamedResourceExtentInspection, VerbatimSourceInspection,
};

static NEXT_BATCH_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputSlot {
    token: u64,
    ordinal: usize,
}

impl OutputSlot {
    /// Stable ordinal within the artifact set that minted this capability.
    #[must_use]
    pub const fn ordinal(self) -> usize {
        self.ordinal
    }
}

impl fmt::Debug for OutputSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutputSlot")
            .field("ordinal", &self.ordinal)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArtifactHandle {
    token: u64,
    ordinal: usize,
}

impl ArtifactHandle {
    /// Stable ordinal within the artifact set that minted this capability.
    #[must_use]
    pub const fn ordinal(self) -> usize {
        self.ordinal
    }
}

impl fmt::Debug for ArtifactHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactHandle")
            .field("ordinal", &self.ordinal)
            .finish_non_exhaustive()
    }
}

struct OutputDeclaration {
    name: LogicalArtifactName,
    artifact_ordinal: Option<usize>,
}

struct OutputBinding {
    name: LogicalArtifactName,
    artifact_ordinal: usize,
}

struct ArtifactNode {
    artifact: PreparedArtifact,
    dependencies: Vec<usize>,
    bound_to_output: bool,
    reachable: bool,
}

/// First phase of a prepared-artifact batch, where the complete public namespace is declared.
pub struct ArtifactBatchDeclaration<'artifact, 'inspection> {
    transaction: ArtifactBudgetTransaction<'artifact>,
    inspection_budget: &'inspection mut AssetLoadBudget,
    token: u64,
    outputs: Vec<OutputDeclaration>,
    output_scratch: ScratchAllocation,
    failed: bool,
}

impl fmt::Debug for ArtifactBatchDeclaration<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactBatchDeclaration")
            .field("output_count", &self.outputs.len())
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl<'artifact, 'inspection> ArtifactBatchDeclaration<'artifact, 'inspection> {
    pub fn begin(
        budget: &'artifact mut ArtifactBudget,
        inspection_budget: &'inspection mut AssetLoadBudget,
    ) -> Result<Self, ArtifactBuildError> {
        let token = NEXT_BATCH_TOKEN
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| ArtifactBuildError::BatchTokenExhausted)?;
        let transaction = budget.transaction();
        let output_scratch = transaction.scratch_allocation();
        Ok(Self {
            transaction,
            inspection_budget,
            token,
            outputs: Vec::new(),
            output_scratch,
            failed: false,
        })
    }

    pub fn declare_output(
        &mut self,
        name: LogicalArtifactName,
    ) -> Result<OutputSlot, ArtifactBuildError> {
        if self.failed {
            return Err(ArtifactBuildError::PoisonedDeclaration);
        }
        let result = self.declare_output_inner(name);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn declare_output_inner(
        &mut self,
        name: LogicalArtifactName,
    ) -> Result<OutputSlot, ArtifactBuildError> {
        let name_bytes = name.heap_bytes()?;
        self.output_scratch
            .grow_vec(&mut self.outputs, 1, "artifact output declarations")?;
        self.transaction.reserve_output_declaration(name_bytes)?;
        let slot = OutputSlot {
            token: self.token,
            ordinal: self.outputs.len(),
        };
        self.outputs.push(OutputDeclaration {
            name,
            artifact_ordinal: None,
        });
        Ok(slot)
    }

    pub fn seal_output_names(
        self,
    ) -> Result<ArtifactBatch<'artifact, 'inspection>, ArtifactBuildError> {
        if self.failed {
            return Err(ArtifactBuildError::PoisonedDeclaration);
        }
        if self.outputs.len() > 1 {
            let mut ordinals = Vec::new();
            let mut scratch = self.transaction.scratch_allocation();
            scratch.grow_vec(&mut ordinals, self.outputs.len(), "artifact name ordinals")?;
            ordinals.extend(0..self.outputs.len());
            validate_unique_names(&mut ordinals, |ordinal| &self.outputs[ordinal].name)?;
        }

        Ok(ArtifactBatch {
            transaction: self.transaction,
            inspection_budget: self.inspection_budget,
            token: self.token,
            outputs: self.outputs,
            output_scratch: self.output_scratch,
            artifacts: Vec::new(),
            failed: false,
        })
    }
}

/// Sealed namespace plus an append-only, leaf-to-root proof-image graph.
pub struct ArtifactBatch<'artifact, 'inspection> {
    transaction: ArtifactBudgetTransaction<'artifact>,
    inspection_budget: &'inspection mut AssetLoadBudget,
    token: u64,
    outputs: Vec<OutputDeclaration>,
    output_scratch: ScratchAllocation,
    artifacts: Vec<ArtifactNode>,
    failed: bool,
}

/// Fallible streamed-resource layout construction tied to one artifact transaction.
pub(crate) struct StreamedResourceLayoutBuilder {
    extents: Vec<StreamedResourceExtentInspection>,
    scratch: ScratchAllocation,
    failed: bool,
}

/// Budgeted generated-byte sink for one YAML artifact leaf.
///
/// The sink is bound to its originating batch. Its allocation remains scratch until
/// [`ArtifactBatch::prepare_yaml_writer`] validates and promotes it into the prepared graph.
pub struct YamlArtifactWriter {
    token: u64,
    writer: GeneratedChunkWriter,
}

impl Write for YamlArtifactWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.writer.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

/// One generated allocation paired with the exact prior artifacts read to derive it.
pub(crate) struct DerivedGeneratedChunk {
    token: u64,
    payload: ArtifactPayload,
    dependencies: Vec<usize>,
    _dependency_scratch: ScratchAllocation,
}

impl DerivedGeneratedChunk {
    pub(crate) const fn len(&self) -> u64 {
        self.payload.len()
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

/// Fallible derivation context for variable-length encoded chunks.
pub(crate) struct DerivedGeneratedChunkEncoder<'encode, 'budget> {
    transaction: &'encode mut ArtifactBudgetTransaction<'budget>,
    token: u64,
    ordinal: usize,
    prior_artifacts: &'encode [ArtifactNode],
    dependencies: Vec<usize>,
    dependency_scratch: ScratchAllocation,
    payload: Option<ArtifactPayload>,
    failed: bool,
}

impl StreamedResourceLayoutBuilder {
    pub(crate) fn push(
        &mut self,
        extent: StreamedResourceExtentInspection,
    ) -> Result<(), ArtifactBuildError> {
        if self.failed {
            return Err(ArtifactBuildError::PoisonedStreamedResourceLayout);
        }
        if let Err(error) = self
            .scratch
            .grow_vec(&mut self.extents, 1, "streamed resource extents")
        {
            self.failed = true;
            return Err(error.into());
        }
        self.extents.push(extent);
        Ok(())
    }
}

impl fmt::Debug for ArtifactBatch<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactBatch")
            .field("output_count", &self.outputs.len())
            .field("proof_image_count", &self.artifacts.len())
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl<'artifact, 'inspection> ArtifactBatch<'artifact, 'inspection> {
    /// Returns the exact length of a previously prepared artifact in this batch.
    pub fn artifact_len(&self, artifact: ArtifactHandle) -> Result<u64, ArtifactBuildError> {
        let ordinal = self.validate_artifact_handle(artifact)?;
        Ok(self.artifacts[ordinal].artifact.len())
    }

    /// Runs one fallible operation as a fail-stop validation boundary for this batch.
    ///
    /// Any returned error poisons the batch, including domain errors produced outside the
    /// artifact crate. This prevents callers from continuing after partially validating or
    /// extending the prepared artifact graph.
    pub fn run_fail_stop<T, E>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<ArtifactBuildError>,
    {
        self.ensure_active().map_err(E::from)?;
        let result = operation(self);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Runs one bounded source-inspection step against this batch's caller-owned load budget.
    ///
    /// The budget borrow exists only for the duration of `inspect`. Any inspection failure poisons
    /// the batch so callers cannot continue from a partially validated artifact graph.
    pub fn inspect_with_budget<T>(
        &mut self,
        inspect: impl for<'budget> FnOnce(&'budget mut AssetLoadBudget) -> Result<T, ArtifactBuildError>,
    ) -> Result<T, ArtifactBuildError> {
        self.ensure_active()?;
        let result = inspect(self.inspection_budget);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub(crate) fn consume_inspection_bytes(
        &mut self,
        amount: u64,
    ) -> Result<(), ArtifactBuildError> {
        self.ensure_active()?;
        let result = self
            .inspection_budget
            .consume_bytes(amount)
            .map_err(ArtifactBuildError::from);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub(crate) fn streamed_resource_layout_builder(
        &self,
    ) -> Result<StreamedResourceLayoutBuilder, ArtifactBuildError> {
        self.ensure_active()?;
        Ok(StreamedResourceLayoutBuilder {
            extents: Vec::new(),
            scratch: self.transaction.scratch_allocation(),
            failed: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn generated_chunk_writer(
        &self,
    ) -> Result<GeneratedChunkWriter, ArtifactBuildError> {
        self.ensure_active()?;
        Ok(GeneratedChunkWriter::new(&self.transaction))
    }

    #[cfg(test)]
    pub(crate) fn finish_generated_chunk(
        &mut self,
        writer: GeneratedChunkWriter,
    ) -> Result<ArtifactPayload, ArtifactBuildError> {
        self.ensure_active()?;
        let result = writer.finish(&mut self.transaction).map_err(Into::into);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub(crate) fn derive_generated_chunk(
        &mut self,
        derive: impl FnOnce(
            &mut DerivedGeneratedChunkEncoder<'_, 'artifact>,
        ) -> Result<(), ArtifactBuildError>,
    ) -> Result<DerivedGeneratedChunk, ArtifactBuildError> {
        self.ensure_active()?;
        let result = self.derive_generated_chunk_inner(derive);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn derive_generated_chunk_inner(
        &mut self,
        derive: impl FnOnce(
            &mut DerivedGeneratedChunkEncoder<'_, 'artifact>,
        ) -> Result<(), ArtifactBuildError>,
    ) -> Result<DerivedGeneratedChunk, ArtifactBuildError> {
        let dependency_scratch = self.transaction.scratch_allocation();
        let mut encoder = DerivedGeneratedChunkEncoder {
            transaction: &mut self.transaction,
            token: self.token,
            ordinal: self.artifacts.len(),
            prior_artifacts: &self.artifacts,
            dependencies: Vec::new(),
            dependency_scratch,
            payload: None,
            failed: false,
        };
        derive(&mut encoder)?;
        encoder.finish()
    }

    pub(crate) fn prepare_serialized_file(
        &mut self,
        declared_len: u64,
        encode: impl FnOnce(&mut ArtifactEncoder<'_, 'artifact>) -> Result<(), ArtifactBuildError>,
    ) -> Result<ArtifactHandle, ArtifactBuildError> {
        self.prepare(ExpectedArtifactFormat::SerializedFile, declared_len, encode)
    }

    pub(crate) fn prepare_asset_bundle(
        &mut self,
        declared_len: u64,
        encode: impl FnOnce(&mut ArtifactEncoder<'_, 'artifact>) -> Result<(), ArtifactBuildError>,
    ) -> Result<ArtifactHandle, ArtifactBuildError> {
        self.prepare(ExpectedArtifactFormat::AssetBundle, declared_len, encode)
    }

    pub(crate) fn prepare_web_file(
        &mut self,
        declared_len: u64,
        encode: impl FnOnce(&mut ArtifactEncoder<'_, 'artifact>) -> Result<(), ArtifactBuildError>,
    ) -> Result<ArtifactHandle, ArtifactBuildError> {
        self.prepare(ExpectedArtifactFormat::WebFile, declared_len, encode)
    }

    /// Retains one complete verified source image as an unchanged artifact leaf.
    ///
    /// The prepared image shares the source allocation, records the exact source dependency, and
    /// binds its proof to the source identity and fingerprint. Generated payloads are rejected so
    /// callers cannot label newly encoded bytes as verbatim source bytes.
    pub fn prepare_verbatim_source(
        &mut self,
        payload: &ArtifactPayload,
    ) -> Result<ArtifactHandle, ArtifactBuildError> {
        self.ensure_active()?;
        let ArtifactPayloadProvenance::Source {
            source_id,
            fingerprint,
        } = payload.provenance()
        else {
            self.failed = true;
            return Err(ArtifactBuildError::VerbatimSourceRequiresSourcePayload);
        };
        let proof = VerbatimSourceInspection::new(source_id, fingerprint, payload.len());
        self.prepare(
            ExpectedArtifactFormat::VerbatimSource(proof),
            payload.len(),
            |encoder| encoder.push_payload_full(payload),
        )
    }

    /// Independently reparses one complete YAML payload into a prepared YAML leaf.
    ///
    /// Source-backed payloads must carry YAML source identity. The image remains segmented and is
    /// parsed without first materializing a contiguous copy.
    pub fn prepare_yaml(
        &mut self,
        payload: &ArtifactPayload,
    ) -> Result<ArtifactHandle, ArtifactBuildError> {
        self.ensure_active()?;
        if let ArtifactPayloadProvenance::Source { source_id, .. } = payload.provenance()
            && source_id.kind() != unity_asset_core::SourceKind::Yaml
        {
            self.failed = true;
            return Err(ArtifactBuildError::YamlSourceKindMismatch { source_id });
        }
        self.prepare_yaml_encoded(payload.len(), |encoder| encoder.push_payload_full(payload))
    }

    /// Creates a caller-owned writer whose generated storage is charged to this batch.
    pub fn yaml_writer(&self) -> Result<YamlArtifactWriter, ArtifactBuildError> {
        self.ensure_active()?;
        Ok(YamlArtifactWriter {
            token: self.token,
            writer: GeneratedChunkWriter::new_for_encoder(&self.transaction),
        })
    }

    /// Promotes a completed YAML writer and independently reparses its exact generated bytes.
    pub fn prepare_yaml_writer(
        &mut self,
        writer: YamlArtifactWriter,
    ) -> Result<ArtifactHandle, ArtifactBuildError> {
        self.ensure_active()?;
        if writer.token != self.token {
            self.failed = true;
            return Err(ArtifactBuildError::ForeignYamlWriter);
        }
        let payload = match writer.writer.finish(&mut self.transaction) {
            Ok(payload) => payload,
            Err(error) => {
                self.failed = true;
                return Err(error.into());
            }
        };
        self.prepare_yaml_encoded(payload.len(), |encoder| encoder.push_payload_full(&payload))
    }

    pub(crate) fn prepare_yaml_encoded(
        &mut self,
        declared_len: u64,
        encode: impl FnOnce(&mut ArtifactEncoder<'_, 'artifact>) -> Result<(), ArtifactBuildError>,
    ) -> Result<ArtifactHandle, ArtifactBuildError> {
        self.prepare(ExpectedArtifactFormat::Yaml, declared_len, encode)
    }

    pub(crate) fn prepare_streamed_resource(
        &mut self,
        builder: StreamedResourceLayoutBuilder,
        encode: impl FnOnce(&mut ArtifactEncoder<'_, 'artifact>) -> Result<(), ArtifactBuildError>,
    ) -> Result<ArtifactHandle, ArtifactBuildError> {
        self.ensure_active()?;
        let result = self.prepare_streamed_resource_inner(builder, encode);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn prepare_streamed_resource_inner(
        &mut self,
        builder: StreamedResourceLayoutBuilder,
        encode: impl FnOnce(&mut ArtifactEncoder<'_, 'artifact>) -> Result<(), ArtifactBuildError>,
    ) -> Result<ArtifactHandle, ArtifactBuildError> {
        if builder.failed {
            return Err(ArtifactBuildError::PoisonedStreamedResourceLayout);
        }
        if !self
            .transaction
            .owns_scratch_ledger(builder.scratch.ledger())
        {
            return Err(ArtifactBuildError::ForeignStreamedResourceLayout);
        }

        let StreamedResourceLayoutBuilder {
            extents,
            mut scratch,
            failed: _,
        } = builder;
        let mut layout = StreamedResourceLayoutProof::from_builder_extents(extents)?;
        let inspection_metadata = layout.retained_heap_bytes()?;
        scratch.validate_for_retention(inspection_metadata)?;
        self.transaction
            .reserve_metadata_bytes(inspection_metadata)?;
        layout.mark_inspection_metadata_precharged(inspection_metadata)?;
        scratch.release_for_retention();
        let declared_len = layout.length();
        self.prepare(
            ExpectedArtifactFormat::StreamedResource(layout),
            declared_len,
            encode,
        )
    }

    #[cfg(test)]
    pub(crate) fn prepare_streamed_resource_extents(
        &mut self,
        extents: impl IntoIterator<Item = StreamedResourceExtentInspection>,
        encode: impl FnOnce(&mut ArtifactEncoder<'_, 'artifact>) -> Result<(), ArtifactBuildError>,
    ) -> Result<ArtifactHandle, ArtifactBuildError> {
        let mut builder = self.streamed_resource_layout_builder()?;
        for extent in extents {
            if let Err(error) = builder.push(extent) {
                self.failed = true;
                return Err(error);
            }
        }
        self.prepare_streamed_resource(builder, encode)
    }

    fn prepare(
        &mut self,
        expected: ExpectedArtifactFormat,
        declared_len: u64,
        encode: impl FnOnce(&mut ArtifactEncoder<'_, 'artifact>) -> Result<(), ArtifactBuildError>,
    ) -> Result<ArtifactHandle, ArtifactBuildError> {
        self.ensure_active()?;
        let result = self.prepare_inner(expected, declared_len, encode);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn prepare_inner(
        &mut self,
        expected: ExpectedArtifactFormat,
        declared_len: u64,
        encode: impl FnOnce(&mut ArtifactEncoder<'_, 'artifact>) -> Result<(), ArtifactBuildError>,
    ) -> Result<ArtifactHandle, ArtifactBuildError> {
        let ordinal = self.artifacts.len();
        self.transaction.reserve_proof_images(1)?;
        self.transaction
            .grow_retained_vec(&mut self.artifacts, 1, "artifact proof records")?;

        let image = ImageBuilder::new(&mut self.transaction, declared_len)?;
        let mut encoder = ArtifactEncoder {
            image,
            token: self.token,
            ordinal,
            prior_artifacts: &self.artifacts,
            dependencies: Vec::new(),
            failed: false,
        };
        encode(&mut encoder)?;
        let (sealed, dependency_ordinals) = encoder.finish()?;
        let (format, preaccounted_inspection_bytes) = PreparedArtifactFormat::inspect(
            expected,
            sealed.segmented(),
            sealed.digest(),
            sealed.dependencies(),
            self.inspection_budget,
        )
        .map_err(ArtifactBuildError::independent_reparse)?;
        let inspection_bytes = format.retained_heap_bytes()?;
        let newly_accounted_inspection_bytes = inspection_bytes
            .checked_sub(preaccounted_inspection_bytes)
            .ok_or(ArtifactBuildError::InternalInvariant {
                message: "preaccounted inspection metadata exceeds retained inspection metadata",
            })?;
        self.transaction
            .reserve_metadata_bytes(newly_accounted_inspection_bytes)?;
        let image = sealed.into_validated(inspection_bytes)?;
        let handle = ArtifactHandle {
            token: self.token,
            ordinal,
        };
        self.artifacts.push(ArtifactNode {
            artifact: PreparedArtifact { format, image },
            dependencies: dependency_ordinals,
            bound_to_output: false,
            reachable: false,
        });
        Ok(handle)
    }

    pub fn bind_output(
        &mut self,
        slot: OutputSlot,
        artifact: ArtifactHandle,
    ) -> Result<(), ArtifactBuildError> {
        self.ensure_active()?;
        let result = self.bind_output_inner(slot, artifact);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn bind_output_inner(
        &mut self,
        slot: OutputSlot,
        artifact: ArtifactHandle,
    ) -> Result<(), ArtifactBuildError> {
        let slot_ordinal = self.validate_output_slot(slot)?;
        let artifact_ordinal = self.validate_artifact_handle(artifact)?;
        if self.outputs[slot_ordinal].artifact_ordinal.is_some() {
            return Err(ArtifactBuildError::OutputAlreadyBound {
                output: slot_ordinal,
            });
        }
        if self.artifacts[artifact_ordinal].bound_to_output {
            return Err(ArtifactBuildError::ArtifactAlreadyBound {
                artifact: artifact_ordinal,
            });
        }
        let publication_bytes = self.artifacts[artifact_ordinal].artifact.len();
        self.transaction
            .reserve_publication_bytes(publication_bytes)?;
        self.outputs[slot_ordinal].artifact_ordinal = Some(artifact_ordinal);
        self.artifacts[artifact_ordinal].bound_to_output = true;
        Ok(())
    }

    pub fn finish(mut self) -> Result<PreparedArtifactSet, ArtifactBuildError> {
        self.ensure_active()?;
        for (ordinal, output) in self.outputs.iter().enumerate() {
            if output.artifact_ordinal.is_none() {
                return Err(ArtifactBuildError::UnboundOutput { output: ordinal });
            }
        }

        for output in &self.outputs {
            let ordinal = output
                .artifact_ordinal
                .ok_or(ArtifactBuildError::InternalInvariant {
                    message: "bound output lost its artifact ordinal",
                })?;
            self.artifacts[ordinal].reachable = true;
        }
        for ordinal in (0..self.artifacts.len()).rev() {
            let (earlier, current_and_later) = self.artifacts.split_at_mut(ordinal);
            let current = &current_and_later[0];
            if current.reachable {
                for dependency in &current.dependencies {
                    earlier[*dependency].reachable = true;
                }
            }
        }
        if let Some((ordinal, _)) = self
            .artifacts
            .iter()
            .enumerate()
            .find(|(_, artifact)| !artifact.reachable)
        {
            return Err(ArtifactBuildError::UnreachableArtifact { artifact: ordinal });
        }

        let build_counters = self
            .artifacts
            .iter()
            .try_fold(ArtifactBuildCounters::default(), |counters, node| {
                counters.checked_add(node.artifact.build_counters())
            })?;
        let source_dependency_count = self.artifacts.iter().try_fold(0_usize, |count, node| {
            count
                .checked_add(node.artifact.source_dependencies().len())
                .ok_or(ArtifactBuildError::ArithmeticOverflow {
                    resource: "artifact_set_source_dependency_count",
                })
        })?;
        let mut source_dependencies = Vec::new();
        self.transaction.grow_retained_vec(
            &mut source_dependencies,
            source_dependency_count,
            "artifact set source dependencies",
        )?;
        for node in &self.artifacts {
            source_dependencies.extend_from_slice(node.artifact.source_dependencies());
        }
        source_dependencies.sort_unstable_by_key(|dependency| dependency.source);
        let mut write = 0_usize;
        for read in 0..source_dependencies.len() {
            let current = source_dependencies[read];
            if write != 0 && source_dependencies[write - 1].source == current.source {
                let previous = &mut source_dependencies[write - 1];
                if previous.fingerprint != current.fingerprint {
                    return Err(ArtifactBuildError::ConflictingSourceFingerprint {
                        source_id: Box::new(current.source),
                        first: previous.fingerprint.digest(),
                        second: current.fingerprint.digest(),
                    });
                }
                previous.referenced_bytes = checked_add(
                    previous.referenced_bytes,
                    current.referenced_bytes,
                    "artifact_set_source_dependency_bytes",
                )?;
            } else {
                if write != read {
                    source_dependencies[write] = current;
                }
                write += 1;
            }
        }
        source_dependencies.truncate(write);
        let referenced_source_bytes =
            source_dependencies
                .iter()
                .try_fold(0_u64, |bytes, dependency| {
                    checked_add(
                        bytes,
                        dependency.referenced_bytes,
                        "artifact_set_referenced_source_bytes",
                    )
                })?;
        let mut output_bindings = Vec::new();
        self.transaction.grow_retained_vec(
            &mut output_bindings,
            self.outputs.len(),
            "committed artifact output bindings",
        )?;
        for output in self.outputs {
            let artifact_ordinal =
                output
                    .artifact_ordinal
                    .ok_or(ArtifactBuildError::InternalInvariant {
                        message: "validated output lost its artifact binding",
                    })?;
            output_bindings.push(OutputBinding {
                name: output.name,
                artifact_ordinal,
            });
        }
        drop(self.output_scratch);

        let pending = self.transaction.pending_usage();
        let footprint = ArtifactSetFootprint {
            outputs: pending.outputs(),
            proof_images: pending.proof_images(),
            publication_bytes: pending.publication_bytes(),
            proof_bytes: pending.proof_bytes(),
            generated_bytes: pending.generated_bytes(),
            metadata_bytes: pending.metadata_bytes(),
            pinned_source_bytes: pending.pinned_source_bytes(),
            retained_bytes: pending.retained_bytes(),
            referenced_source_bytes,
            segments: pending.segments(),
        };

        let set = PreparedArtifactSet {
            token: self.token,
            outputs: output_bindings,
            artifacts: self.artifacts,
            source_dependencies,
            footprint,
            build_counters,
        };
        self.transaction.commit()?;
        Ok(set)
    }

    fn validate_output_slot(&self, slot: OutputSlot) -> Result<usize, ArtifactBuildError> {
        if slot.token != self.token {
            return Err(ArtifactBuildError::ForeignOutputSlot {
                output: slot.ordinal,
            });
        }
        if slot.ordinal >= self.outputs.len() {
            return Err(ArtifactBuildError::UnknownOutputSlot {
                output: slot.ordinal,
            });
        }
        Ok(slot.ordinal)
    }

    fn validate_artifact_handle(
        &self,
        artifact: ArtifactHandle,
    ) -> Result<usize, ArtifactBuildError> {
        if artifact.token != self.token {
            return Err(ArtifactBuildError::ForeignArtifactHandle {
                artifact: artifact.ordinal,
            });
        }
        if artifact.ordinal >= self.artifacts.len() {
            return Err(ArtifactBuildError::UnknownArtifactHandle {
                artifact: artifact.ordinal,
            });
        }
        Ok(artifact.ordinal)
    }

    fn ensure_active(&self) -> Result<(), ArtifactBuildError> {
        if self.failed {
            Err(ArtifactBuildError::PoisonedBatch)
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    pub(crate) const fn pending_usage(&self) -> super::ArtifactBudgetUsage {
        self.transaction.pending_usage()
    }
}

/// Crate-private encoder capability scoped to one append-only artifact ordinal.
pub(crate) struct ArtifactEncoder<'encode, 'budget> {
    image: ImageBuilder<'encode, 'budget>,
    token: u64,
    ordinal: usize,
    prior_artifacts: &'encode [ArtifactNode],
    dependencies: Vec<usize>,
    failed: bool,
}

impl ArtifactEncoder<'_, '_> {
    pub(crate) fn generated_chunk_writer(
        &self,
    ) -> Result<GeneratedChunkWriter, ArtifactBuildError> {
        self.ensure_active()?;
        Ok(self.image.generated_chunk_writer())
    }

    pub(crate) fn finish_generated_chunk(
        &mut self,
        writer: GeneratedChunkWriter,
    ) -> Result<ArtifactPayload, ArtifactBuildError> {
        self.ensure_active()?;
        let result = self.image.finish_generated_chunk(writer);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub(crate) fn push_payload_full(
        &mut self,
        payload: &ArtifactPayload,
    ) -> Result<(), ArtifactBuildError> {
        self.ensure_active()?;
        let result = self.image.push_payload_full(payload);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub(crate) fn push_payload_range(
        &mut self,
        payload: &ArtifactPayload,
        range: Range<usize>,
    ) -> Result<(), ArtifactBuildError> {
        self.ensure_active()?;
        let result = self.image.push_payload_range(payload, range);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub(crate) fn push_derived_generated_chunk(
        &mut self,
        chunk: DerivedGeneratedChunk,
    ) -> Result<(), ArtifactBuildError> {
        self.ensure_active()?;
        let result = self.push_derived_generated_chunk_inner(chunk);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn push_derived_generated_chunk_inner(
        &mut self,
        chunk: DerivedGeneratedChunk,
    ) -> Result<(), ArtifactBuildError> {
        if chunk.token != self.token {
            return Err(ArtifactBuildError::ForeignDerivedGeneratedChunk);
        }
        if chunk.is_empty() {
            return Err(ArtifactBuildError::EmptyDerivedGeneratedChunk);
        }
        self.image
            .reserve_graph_edges(&mut self.dependencies, chunk.dependencies.len())?;
        self.image.push_payload_full(&chunk.payload)?;
        self.dependencies.extend_from_slice(&chunk.dependencies);
        Ok(())
    }

    pub(crate) fn append_dependency(
        &mut self,
        handle: ArtifactHandle,
    ) -> Result<(), ArtifactBuildError> {
        self.ensure_active()?;
        let ordinal = match self.validate_dependency_handle(handle) {
            Ok(ordinal) => ordinal,
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        };
        if let Err(error) = self.image.reserve_graph_edges(&mut self.dependencies, 1) {
            self.failed = true;
            return Err(error);
        }
        if let Err(error) = self
            .image
            .append_validated_full(&self.prior_artifacts[ordinal].artifact.image)
        {
            self.failed = true;
            return Err(error);
        }
        self.dependencies.push(ordinal);
        Ok(())
    }

    pub(crate) fn append_dependency_range(
        &mut self,
        handle: ArtifactHandle,
        range: Range<u64>,
    ) -> Result<(), ArtifactBuildError> {
        self.ensure_active()?;
        let ordinal = match self.validate_dependency_handle(handle) {
            Ok(ordinal) => ordinal,
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        };
        if range.is_empty() {
            self.failed = true;
            return Err(ArtifactBuildError::EmptyDependencyUse { artifact: ordinal });
        }
        if let Err(error) = self.image.reserve_graph_edges(&mut self.dependencies, 1) {
            self.failed = true;
            return Err(error);
        }
        if let Err(error) = self
            .image
            .append_validated_range(&self.prior_artifacts[ordinal].artifact.image, range)
        {
            self.failed = true;
            return Err(error);
        }
        self.dependencies.push(ordinal);
        Ok(())
    }

    fn validate_dependency_handle(
        &self,
        handle: ArtifactHandle,
    ) -> Result<usize, ArtifactBuildError> {
        if handle.token != self.token {
            return Err(ArtifactBuildError::ForeignArtifactHandle {
                artifact: handle.ordinal,
            });
        }
        if handle.ordinal >= self.ordinal || handle.ordinal >= self.prior_artifacts.len() {
            return Err(ArtifactBuildError::DependencyMustPrecedeArtifact {
                dependency: handle.ordinal,
                artifact: self.ordinal,
            });
        }
        Ok(handle.ordinal)
    }

    fn ensure_active(&self) -> Result<(), ArtifactBuildError> {
        if self.failed || self.image.transaction_is_poisoned() {
            Err(ArtifactBuildError::PoisonedEncoder)
        } else {
            Ok(())
        }
    }

    fn finish(mut self) -> Result<(SealedImage, Vec<usize>), ArtifactBuildError> {
        self.ensure_active()?;
        self.dependencies.sort_unstable();
        self.dependencies.dedup();
        Ok((self.image.seal()?, self.dependencies))
    }
}

impl DerivedGeneratedChunkEncoder<'_, '_> {
    pub(crate) fn codec_scratch_budget(&self) -> CodecScratchBudget {
        self.transaction.codec_scratch_budget()
    }

    pub(crate) fn generated_chunk_writer(
        &self,
    ) -> Result<GeneratedChunkWriter, ArtifactBuildError> {
        self.ensure_active()?;
        Ok(GeneratedChunkWriter::new_for_encoder(self.transaction))
    }

    pub(crate) fn finish_generated_chunk(
        &mut self,
        writer: GeneratedChunkWriter,
    ) -> Result<(), ArtifactBuildError> {
        self.ensure_active()?;
        if self.payload.is_some() {
            self.failed = true;
            return Err(ArtifactBuildError::DerivedGeneratedChunkAlreadyFinished);
        }
        match writer.finish(self.transaction) {
            Ok(payload) => {
                self.payload = Some(payload);
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error.into())
            }
        }
    }

    pub(crate) fn dependency_reader(
        &mut self,
        handle: ArtifactHandle,
    ) -> Result<DependencyReader<'_>, ArtifactBuildError> {
        self.ensure_active()?;
        let ordinal = match self.validate_dependency_handle(handle) {
            Ok(ordinal) => ordinal,
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        };
        if let Err(error) = self.dependency_scratch.grow_vec(
            &mut self.dependencies,
            1,
            "derived generated chunk dependencies",
        ) {
            self.failed = true;
            return Err(error.into());
        }
        Ok(DependencyReader {
            reader: self.prior_artifacts[ordinal].artifact.reader(),
            dependencies: &mut self.dependencies,
            ordinal,
            recorded: false,
            encoder_failed: &mut self.failed,
        })
    }

    pub(crate) fn record_empty_dependency(
        &mut self,
        handle: ArtifactHandle,
    ) -> Result<(), ArtifactBuildError> {
        self.ensure_active()?;
        let ordinal = match self.validate_dependency_handle(handle) {
            Ok(ordinal) => ordinal,
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        };
        if !self.prior_artifacts[ordinal].artifact.is_empty() {
            self.failed = true;
            return Err(ArtifactBuildError::NonEmptyDependencyWithoutRead { artifact: ordinal });
        }
        if let Err(error) = self.dependency_scratch.grow_vec(
            &mut self.dependencies,
            1,
            "derived generated chunk dependencies",
        ) {
            self.failed = true;
            return Err(error.into());
        }
        self.dependencies.push(ordinal);
        Ok(())
    }

    fn validate_dependency_handle(
        &self,
        handle: ArtifactHandle,
    ) -> Result<usize, ArtifactBuildError> {
        if handle.token != self.token {
            return Err(ArtifactBuildError::ForeignArtifactHandle {
                artifact: handle.ordinal,
            });
        }
        if handle.ordinal >= self.ordinal || handle.ordinal >= self.prior_artifacts.len() {
            return Err(ArtifactBuildError::DependencyMustPrecedeArtifact {
                dependency: handle.ordinal,
                artifact: self.ordinal,
            });
        }
        Ok(handle.ordinal)
    }

    fn ensure_active(&self) -> Result<(), ArtifactBuildError> {
        if self.failed || self.transaction.transaction_is_poisoned() {
            Err(ArtifactBuildError::PoisonedDerivedGeneratedChunk)
        } else {
            Ok(())
        }
    }

    fn finish(mut self) -> Result<DerivedGeneratedChunk, ArtifactBuildError> {
        self.ensure_active()?;
        let payload = self
            .payload
            .take()
            .ok_or(ArtifactBuildError::UnfinishedDerivedGeneratedChunk)?;
        self.dependencies.sort_unstable();
        self.dependencies.dedup();
        Ok(DerivedGeneratedChunk {
            token: self.token,
            payload,
            dependencies: self.dependencies,
            _dependency_scratch: self.dependency_scratch,
        })
    }
}

/// Reader that records a proof edge only after bytes are actually consumed.
pub(crate) struct DependencyReader<'artifact> {
    reader: ArtifactReader<'artifact>,
    dependencies: &'artifact mut Vec<usize>,
    ordinal: usize,
    recorded: bool,
    encoder_failed: &'artifact mut bool,
}

impl Read for DependencyReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let read = match self.reader.read(output) {
            Ok(read) => read,
            Err(error) => {
                *self.encoder_failed = true;
                return Err(error);
            }
        };
        if read != 0 && !self.recorded {
            self.dependencies.push(self.ordinal);
            self.recorded = true;
        }
        Ok(read)
    }
}

impl Seek for DependencyReader<'_> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        match self.reader.seek(position) {
            Ok(offset) => Ok(offset),
            Err(error) => {
                *self.encoder_failed = true;
                Err(error)
            }
        }
    }
}

/// One path-independent exact byte image and its parser-produced proof.
pub struct PreparedArtifact {
    format: PreparedArtifactFormat,
    image: ValidatedImage,
}

impl PreparedArtifact {
    #[must_use]
    pub const fn format(&self) -> &PreparedArtifactFormat {
        &self.format
    }

    #[must_use]
    pub const fn len(&self) -> u64 {
        self.image.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub const fn digest(&self) -> DigestV1 {
        self.image.digest()
    }

    #[must_use]
    pub fn source_dependencies(&self) -> &[ArtifactSourceDependency] {
        self.image.dependencies()
    }

    #[must_use]
    pub const fn footprint(&self) -> ArtifactFootprint {
        self.image.footprint()
    }

    #[must_use]
    pub const fn build_counters(&self) -> ArtifactBuildCounters {
        self.image.counters()
    }

    #[must_use]
    pub fn reader(&self) -> ArtifactReader<'_> {
        self.image.reader()
    }

    /// Returns a contained logical range when it occupies one immutable segment.
    ///
    /// Invalid ranges and ranges crossing segment boundaries return `None`. This
    /// method never concatenates or allocates an artifact-sized buffer.
    #[must_use]
    pub fn contiguous_range(&self, range: Range<u64>) -> Option<&[u8]> {
        self.image.contiguous_range(range)
    }

    pub fn stream_verified_to(
        &self,
        sink: &mut impl std::io::Write,
    ) -> Result<ArtifactStreamReceipt, ArtifactStreamError> {
        self.image.stream_verified_to(sink)
    }
}

impl fmt::Debug for PreparedArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedArtifact")
            .field("format", &self.format)
            .field("digest", &self.digest())
            .field("footprint", &self.footprint())
            .finish_non_exhaustive()
    }
}

/// A complete, atomically committed graph of proof images and public output roots.
pub struct PreparedArtifactSet {
    token: u64,
    outputs: Vec<OutputBinding>,
    artifacts: Vec<ArtifactNode>,
    source_dependencies: Vec<ArtifactSourceDependency>,
    footprint: ArtifactSetFootprint,
    build_counters: ArtifactBuildCounters,
}

impl PreparedArtifactSet {
    /// Returns the public output bound to this exact declaration slot.
    ///
    /// Slots are batch capabilities. A slot produced by another graph is rejected even when its
    /// numeric ordinal happens to exist in this set.
    pub fn output(&self, slot: OutputSlot) -> Result<PreparedOutput<'_>, ArtifactBuildError> {
        if slot.token != self.token {
            return Err(ArtifactBuildError::ForeignOutputSlot {
                output: slot.ordinal,
            });
        }
        let output =
            self.outputs
                .get(slot.ordinal)
                .ok_or(ArtifactBuildError::UnknownOutputSlot {
                    output: slot.ordinal,
                })?;
        let artifact = self.artifacts.get(output.artifact_ordinal).ok_or(
            ArtifactBuildError::InternalInvariant {
                message: "prepared output references an unknown artifact",
            },
        )?;
        Ok(PreparedOutput {
            slot,
            handle: ArtifactHandle {
                token: self.token,
                ordinal: output.artifact_ordinal,
            },
            name: &output.name,
            artifact: &artifact.artifact,
        })
    }

    /// Returns an artifact from this exact prepared graph.
    ///
    /// Handles are batch capabilities. A handle produced by another graph is rejected even when
    /// its numeric ordinal happens to exist in this set.
    pub fn artifact(
        &self,
        handle: ArtifactHandle,
    ) -> Result<&PreparedArtifact, ArtifactBuildError> {
        if handle.token != self.token {
            return Err(ArtifactBuildError::ForeignArtifactHandle {
                artifact: handle.ordinal,
            });
        }
        self.artifacts
            .get(handle.ordinal)
            .map(|node| &node.artifact)
            .ok_or(ArtifactBuildError::UnknownArtifactHandle {
                artifact: handle.ordinal,
            })
    }

    #[must_use]
    pub fn outputs(&self) -> PreparedOutputIter<'_> {
        PreparedOutputIter {
            token: self.token,
            outputs: self.outputs.iter(),
            artifacts: &self.artifacts,
            next_ordinal: 0,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.outputs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }

    #[must_use]
    pub fn proof_image_count(&self) -> usize {
        self.artifacts.len()
    }

    #[must_use]
    pub fn source_dependencies(&self) -> &[ArtifactSourceDependency] {
        &self.source_dependencies
    }

    #[must_use]
    pub const fn footprint(&self) -> ArtifactSetFootprint {
        self.footprint
    }

    #[must_use]
    pub const fn build_counters(&self) -> ArtifactBuildCounters {
        self.build_counters
    }

    #[cfg(test)]
    pub(crate) fn retained_metadata_bytes_for_test(&self) -> Result<u64, ArtifactBuildError> {
        let mut bytes = vec_allocation_bytes::<OutputBinding>(self.outputs.capacity())
            .map_err(ArtifactBudgetError::from)?;
        bytes = checked_add(
            bytes,
            vec_allocation_bytes::<ArtifactNode>(self.artifacts.capacity())
                .map_err(ArtifactBudgetError::from)?,
            "artifact_set_retained_metadata",
        )?;
        bytes = checked_add(
            bytes,
            vec_allocation_bytes::<ArtifactSourceDependency>(self.source_dependencies.capacity())
                .map_err(ArtifactBudgetError::from)?,
            "artifact_set_retained_metadata",
        )?;
        for output in &self.outputs {
            bytes = checked_add(
                bytes,
                output.name.heap_bytes()?,
                "artifact_set_retained_metadata",
            )?;
        }
        for node in &self.artifacts {
            bytes = checked_add(
                bytes,
                vec_allocation_bytes::<usize>(node.dependencies.capacity())
                    .map_err(ArtifactBudgetError::from)?,
                "artifact_set_retained_metadata",
            )?;
            bytes = checked_add(
                bytes,
                node.artifact.footprint().metadata_bytes(),
                "artifact_set_retained_metadata",
            )?;
        }
        Ok(bytes)
    }
}

impl fmt::Debug for PreparedArtifactSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedArtifactSet")
            .field("output_count", &self.outputs.len())
            .field("proof_image_count", &self.artifacts.len())
            .field("footprint", &self.footprint)
            .field("build_counters", &self.build_counters)
            .finish_non_exhaustive()
    }
}

pub struct PreparedOutput<'artifact> {
    slot: OutputSlot,
    handle: ArtifactHandle,
    name: &'artifact LogicalArtifactName,
    artifact: &'artifact PreparedArtifact,
}

impl<'artifact> PreparedOutput<'artifact> {
    #[must_use]
    pub const fn slot(&self) -> OutputSlot {
        self.slot
    }

    #[must_use]
    pub const fn handle(&self) -> ArtifactHandle {
        self.handle
    }

    #[must_use]
    pub const fn name(&self) -> &'artifact LogicalArtifactName {
        self.name
    }

    #[must_use]
    pub const fn artifact(&self) -> &'artifact PreparedArtifact {
        self.artifact
    }
}

impl fmt::Debug for PreparedOutput<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedOutput")
            .field("slot", &self.slot)
            .field("handle", &self.handle)
            .field("name", &self.name)
            .field("artifact", &self.artifact)
            .finish()
    }
}

pub struct PreparedOutputIter<'artifact> {
    token: u64,
    outputs: slice::Iter<'artifact, OutputBinding>,
    artifacts: &'artifact [ArtifactNode],
    next_ordinal: usize,
}

impl<'artifact> Iterator for PreparedOutputIter<'artifact> {
    type Item = PreparedOutput<'artifact>;

    fn next(&mut self) -> Option<Self::Item> {
        let output = self.outputs.next()?;
        let output_ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        let artifact_ordinal = output.artifact_ordinal;
        Some(PreparedOutput {
            slot: OutputSlot {
                token: self.token,
                ordinal: output_ordinal,
            },
            handle: ArtifactHandle {
                token: self.token,
                ordinal: artifact_ordinal,
            },
            name: &output.name,
            artifact: &self.artifacts[artifact_ordinal].artifact,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.outputs.size_hint()
    }
}

impl ExactSizeIterator for PreparedOutputIter<'_> {}
impl FusedIterator for PreparedOutputIter<'_> {}

/// The externally meaningful stage that rejected an artifact build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArtifactBuildFailurePhase {
    /// Declaration, encoding, artifact budgeting, or final retained-proof construction failed.
    Encoding,
    /// The fixed-format inspector rejected the exact bytes produced by the encoder.
    IndependentReparse,
}

#[derive(Debug, Error)]
pub enum ArtifactBuildError {
    #[error("independent artifact reparse rejected the encoded image: {source}")]
    IndependentReparse {
        #[source]
        source: Box<ArtifactBuildError>,
    },
    #[error(transparent)]
    Name(#[from] ArtifactNameError),
    #[error(transparent)]
    Budget(#[from] ArtifactBudgetError),
    #[error(transparent)]
    Binary(#[from] BinaryError),
    #[error(transparent)]
    LoadBudget(#[from] unity_asset_core::BudgetError),
    #[error(transparent)]
    Digest(#[from] DigestBuildError),
    #[error(transparent)]
    Payload(ArtifactPayloadError),
    #[error("failed to consume a prepared dependency: {0}")]
    DependencyIo(std::io::Error),
    #[error("invalid UTF-8 in YAML artifact at byte offset {offset}")]
    InvalidYamlUtf8 { offset: u64 },
    #[error("invalid YAML artifact: {0}")]
    InvalidYaml(#[source] yaml_rust2::ScanError),
    #[error("{codec} codec failed while attempting to {operation}")]
    CodecFailure {
        codec: &'static str,
        operation: &'static str,
    },
    #[error("artifact batch token space is exhausted")]
    BatchTokenExhausted,
    #[error("artifact arithmetic overflow for {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("artifact backing range {start}..{end} exceeds allocation length {backing_len}")]
    InvalidBackingRange {
        start: usize,
        end: usize,
        backing_len: usize,
    },
    #[error(
        "generated payload ranges must retain the complete allocation; got {start}..{end} of {payload_len} bytes"
    )]
    PartialGeneratedPayload {
        start: usize,
        end: usize,
        payload_len: usize,
    },
    #[error("invalid internal artifact segment state: {message}")]
    InternalSegmentState { message: &'static str },
    #[error("internal artifact invariant failed: {message}")]
    InternalInvariant { message: &'static str },
    #[error("source {source_id:?} has conflicting digests {first} and {second}")]
    ConflictingSourceFingerprint {
        source_id: Box<SourceId>,
        first: DigestV1,
        second: DigestV1,
    },
    #[error("artifact image declared {declared} bytes but retained {actual}")]
    InternalLengthMismatch { declared: u64, actual: u64 },
    #[error("dependency range {start}..{end} exceeds artifact length {artifact_len}")]
    InvalidDependencyRange {
        start: u64,
        end: u64,
        artifact_len: u64,
    },
    #[error("artifact {artifact} cannot establish a dependency edge without consuming bytes")]
    EmptyDependencyUse { artifact: usize },
    #[error("a verbatim-source artifact requires a verified source-backed payload")]
    VerbatimSourceRequiresSourcePayload,
    #[error("YAML artifact payload uses non-YAML source identity {source_id:?}")]
    YamlSourceKindMismatch { source_id: SourceId },
    #[error("YAML artifact writer belongs to a different artifact batch")]
    ForeignYamlWriter,
    #[error("verbatim source proof expected {expected} bytes but retained {actual}")]
    VerbatimSourceLengthMismatch { expected: u64, actual: u64 },
    #[error("verbatim source proof digest mismatch: expected {expected}, got {actual}")]
    VerbatimSourceDigestMismatch {
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error("verbatim source proof does not match the retained source provenance")]
    VerbatimSourceProvenanceMismatch,
    #[error("nonempty artifact {artifact} cannot become a generated dependency without a read")]
    NonEmptyDependencyWithoutRead { artifact: usize },
    #[error("streamed-resource alignment {alignment} must be a nonzero power of two")]
    InvalidStreamedResourceAlignment { alignment: u32 },
    #[error(
        "streamed-resource extent {ordinal} starts at {actual}, but the canonical layout requires {expected}"
    )]
    StreamedResourceExtentOffsetMismatch {
        ordinal: usize,
        expected: u64,
        actual: u64,
    },
    #[error("streamed-resource layout spans {planned} bytes, but its image spans {actual} bytes")]
    StreamedResourceLengthMismatch { planned: u64, actual: u64 },
    #[error("streamed-resource padding before extent {ordinal} contains nonzero bytes")]
    NonZeroStreamedResourcePadding { ordinal: usize },
    #[error(
        "streamed-resource extent {ordinal} digest mismatch: expected {expected}, got {actual}"
    )]
    StreamedResourcePayloadDigestMismatch {
        ordinal: usize,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error("output slot {output} belongs to a different artifact batch")]
    ForeignOutputSlot { output: usize },
    #[error("output slot {output} is not declared by this artifact batch")]
    UnknownOutputSlot { output: usize },
    #[error("artifact handle {artifact} belongs to a different artifact batch")]
    ForeignArtifactHandle { artifact: usize },
    #[error("artifact handle {artifact} is not prepared by this artifact batch")]
    UnknownArtifactHandle { artifact: usize },
    #[error("output slot {output} is already bound")]
    OutputAlreadyBound { output: usize },
    #[error("artifact {artifact} is already bound to an output")]
    ArtifactAlreadyBound { artifact: usize },
    #[error("artifact {artifact} depends on non-earlier artifact {dependency}")]
    DependencyMustPrecedeArtifact { dependency: usize, artifact: usize },
    #[error("output slot {output} has no artifact binding")]
    UnboundOutput { output: usize },
    #[error("artifact {artifact} is not reachable from a public output")]
    UnreachableArtifact { artifact: usize },
    #[error("artifact batch cannot continue after a failed operation")]
    PoisonedBatch,
    #[error("artifact encoder cannot finish after a failed operation")]
    PoisonedEncoder,
    #[error("derived generated chunk encoder cannot finish after a failed operation")]
    PoisonedDerivedGeneratedChunk,
    #[error("derived generated chunk encoding finished without a generated payload")]
    UnfinishedDerivedGeneratedChunk,
    #[error("derived generated chunk encoding finished its payload more than once")]
    DerivedGeneratedChunkAlreadyFinished,
    #[error("derived generated chunk belongs to a different artifact batch")]
    ForeignDerivedGeneratedChunk,
    #[error("a derived generated chunk must contain at least one encoded byte")]
    EmptyDerivedGeneratedChunk,
    #[error("artifact output declaration cannot continue after a failed declaration")]
    PoisonedDeclaration,
    #[error("streamed-resource layout belongs to a different artifact batch")]
    ForeignStreamedResourceLayout,
    #[error("streamed-resource layout cannot continue after a failed operation")]
    PoisonedStreamedResourceLayout,
}

impl ArtifactBuildError {
    /// Reports whether this failure arose while encoding or independently reparsing its result.
    #[must_use]
    pub const fn failure_phase(&self) -> ArtifactBuildFailurePhase {
        match self {
            Self::IndependentReparse { .. } => ArtifactBuildFailurePhase::IndependentReparse,
            _ => ArtifactBuildFailurePhase::Encoding,
        }
    }

    fn independent_reparse(source: Self) -> Self {
        Self::IndependentReparse {
            source: Box::new(source),
        }
    }
}

impl From<ArtifactPayloadError> for ArtifactBuildError {
    fn from(error: ArtifactPayloadError) -> Self {
        match error {
            ArtifactPayloadError::Budget(error) => Self::Budget(error),
            error => Self::Payload(error),
        }
    }
}

impl From<std::io::Error> for ArtifactBuildError {
    fn from(error: std::io::Error) -> Self {
        let kind = error.kind();
        let Some(source) = error.into_inner() else {
            return Self::DependencyIo(std::io::Error::from(kind));
        };
        match source.downcast::<ArtifactPayloadError>() {
            Ok(payload) => match *payload {
                ArtifactPayloadError::Budget(error) => Self::Budget(error),
                error => Self::Payload(error),
            },
            Err(source) => Self::DependencyIo(std::io::Error::new(kind, source)),
        }
    }
}

fn checked_add(left: u64, right: u64, resource: &'static str) -> Result<u64, ArtifactBuildError> {
    left.checked_add(right)
        .ok_or(ArtifactBuildError::ArithmeticOverflow { resource })
}
