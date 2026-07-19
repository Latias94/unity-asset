use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::sync::Arc;

use unity_asset_binary::{ByteSegment, SegmentedBytes};
use unity_asset_core::{
    DigestV1, DigestV1Builder, SourceFingerprint, SourceId, vec_allocation_bytes,
};

use super::budget::{ArtifactBudgetError, ArtifactBudgetTransaction, FallibleTable};
use super::payload::{ArtifactBacking, ArtifactBackingIdentity, GeneratedChunkWriter};
use super::{
    ArtifactBuildCounters, ArtifactBuildError, ArtifactFootprint, ArtifactPayload,
    ArtifactPayloadProvenance, ArtifactSourceDependency, ArtifactStreamError,
    ArtifactStreamReceipt,
};

struct SourceTail {
    source_id: SourceId,
    fingerprint: SourceFingerprint,
    backing: ArtifactBacking,
    range: Range<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImageBackingClass {
    Generated,
    Source,
}

struct ImageBackingUsage {
    class: ImageBackingClass,
    allocation_bytes: u64,
}

#[derive(Clone, Copy)]
enum ArtifactSegmentProvenance {
    Source {
        source_id: SourceId,
        fingerprint: SourceFingerprint,
    },
    Generated,
}

impl ArtifactSegmentProvenance {
    const fn backing_class(self) -> ImageBackingClass {
        match self {
            Self::Source { .. } => ImageBackingClass::Source,
            Self::Generated => ImageBackingClass::Generated,
        }
    }
}

#[derive(Clone)]
struct ArtifactSegmentUsage {
    backing: ArtifactBacking,
    provenance: ArtifactSegmentProvenance,
}

struct DeferredDigestHint {
    backing: ArtifactBacking,
    range: Range<usize>,
    digest: DigestV1,
}

pub(crate) struct ImageBuilder<'transaction, 'budget> {
    budget: &'transaction mut ArtifactBudgetTransaction<'budget>,
    segments: Vec<ByteSegment>,
    segment_usage: Vec<ArtifactSegmentUsage>,
    dependencies: Vec<ArtifactSourceDependency>,
    dependency_indices: FallibleTable<SourceId, usize>,
    backings: FallibleTable<ArtifactBackingIdentity, ImageBackingUsage>,
    source_tail: Option<SourceTail>,
    declared_output_len: u64,
    logical_len: u64,
    digest_builder: DigestV1Builder,
    digest_hint: Option<DeferredDigestHint>,
    referenced_source_bytes: u64,
    owned_generated_bytes: u64,
    pinned_source_bytes: u64,
    source_ranges: u64,
    generated_chunks: u64,
}

impl<'transaction, 'budget> ImageBuilder<'transaction, 'budget> {
    pub(crate) fn new(
        budget: &'transaction mut ArtifactBudgetTransaction<'budget>,
        declared_output_len: u64,
    ) -> Result<Self, ArtifactBuildError> {
        let scratch = budget.scratch_ledger();
        Ok(Self {
            budget,
            segments: Vec::new(),
            segment_usage: Vec::new(),
            dependencies: Vec::new(),
            dependency_indices: FallibleTable::new(Arc::clone(&scratch)),
            backings: FallibleTable::new(scratch),
            source_tail: None,
            declared_output_len,
            logical_len: 0,
            digest_builder: DigestV1Builder::new(declared_output_len),
            digest_hint: None,
            referenced_source_bytes: 0,
            owned_generated_bytes: 0,
            pinned_source_bytes: 0,
            source_ranges: 0,
            generated_chunks: 0,
        })
    }

    pub(crate) fn push_payload_full(
        &mut self,
        payload: &ArtifactPayload,
    ) -> Result<(), ArtifactBuildError> {
        self.push_payload_range(payload, 0..payload.bytes().len())
    }

    pub(crate) fn push_payload_range(
        &mut self,
        payload: &ArtifactPayload,
        range: Range<usize>,
    ) -> Result<(), ArtifactBuildError> {
        validate_backing_range(payload.bytes().len(), &range)?;
        let is_full_payload = range.start == 0 && range.end == payload.bytes().len();
        if range.is_empty() {
            if is_full_payload
                && let ArtifactPayloadProvenance::Source {
                    source_id,
                    fingerprint,
                } = payload.provenance()
            {
                self.budget.reserve_source_proof(source_id, fingerprint)?;
                self.reserve_dependency(source_id, fingerprint, 0)?;
                self.record_digest(payload, range, true)?;
            }
            return Ok(());
        }
        let digest_range = range.clone();
        match payload.provenance() {
            ArtifactPayloadProvenance::Source {
                source_id,
                fingerprint,
            } => self.push_source_range(source_id, fingerprint, payload.backing(), range)?,
            ArtifactPayloadProvenance::Generated => {
                if !is_full_payload {
                    return Err(ArtifactBuildError::PartialGeneratedPayload {
                        start: range.start,
                        end: range.end,
                        payload_len: payload.bytes().len(),
                    });
                }
                self.push_generated(payload.backing())?;
            }
        }
        self.record_digest(payload, digest_range, is_full_payload)?;
        Ok(())
    }

    pub(crate) fn generated_chunk_writer(&self) -> GeneratedChunkWriter {
        GeneratedChunkWriter::new_for_encoder(self.budget)
    }

    pub(crate) fn finish_generated_chunk(
        &mut self,
        writer: GeneratedChunkWriter,
    ) -> Result<ArtifactPayload, ArtifactBuildError> {
        writer.finish(self.budget).map_err(Into::into)
    }

    pub(crate) fn transaction_is_poisoned(&self) -> bool {
        self.budget.transaction_is_poisoned()
    }

    pub(crate) fn reserve_graph_edges(
        &mut self,
        edges: &mut Vec<usize>,
        additional: usize,
    ) -> Result<(), ArtifactBuildError> {
        self.budget
            .grow_retained_vec(edges, additional, "artifact dependency edges")?;
        Ok(())
    }

    pub(crate) fn append_validated_full(
        &mut self,
        image: &ValidatedImage,
    ) -> Result<(), ArtifactBuildError> {
        self.append_validated_range(image, 0..image.len())
    }

    pub(crate) fn append_validated_range(
        &mut self,
        image: &ValidatedImage,
        range: Range<u64>,
    ) -> Result<(), ArtifactBuildError> {
        if range.start > range.end || range.end > image.len() {
            return Err(ArtifactBuildError::InvalidDependencyRange {
                start: range.start,
                end: range.end,
                artifact_len: image.len(),
            });
        }
        if range.is_empty() {
            return Ok(());
        }

        let segments = image.image.segments();
        let first = segments.partition_point(|segment| segment.logical_range().end <= range.start);
        let end = segments.partition_point(|segment| segment.logical_range().start < range.end);
        let selected_segments = &segments[first..end];
        let selected_usage =
            image
                .segment_usage
                .get(first..end)
                .ok_or(ArtifactBuildError::InternalInvariant {
                    message: "artifact byte segments and usage metadata diverged",
                })?;
        let segment_count = selected_segments.len();
        let segment_count_u64 = usize_to_u64(segment_count, "dependency_segment_count")?;
        self.reserve_segment_capacity(segment_count, segment_count_u64)?;
        let destination_start = self.logical_len;
        self.reserve_output(range.end - range.start)?;
        self.flush_deferred_digest()?;

        for (segment, usage) in selected_segments.iter().zip(selected_usage) {
            let logical = segment.logical_range();
            let overlap_start = logical.start.max(range.start);
            let overlap_end = logical.end.min(range.end);
            debug_assert!(overlap_start < overlap_end);
            self.reserve_image_backing(&usage.backing, usage.provenance.backing_class())?;
            let overlap_len = overlap_end - overlap_start;
            if let ArtifactSegmentProvenance::Source {
                source_id,
                fingerprint,
            } = usage.provenance
            {
                self.reserve_dependency(source_id, fingerprint, overlap_len)?;
                self.referenced_source_bytes = checked_add(
                    self.referenced_source_bytes,
                    overlap_len,
                    "referenced_source_bytes",
                )?;
            }
            let rebased_start = checked_add(
                destination_start,
                overlap_start - range.start,
                "dependency_rebased_start",
            )?;
            let rebased = segment.rebase_subrange(overlap_start..overlap_end, rebased_start)?;
            self.digest_builder.update(rebased.as_slice())?;
            self.segments.push(rebased);
            self.segment_usage.push(usage.clone());
        }
        self.source_tail = None;
        Ok(())
    }

    fn push_source_range(
        &mut self,
        source_id: SourceId,
        fingerprint: SourceFingerprint,
        backing: &ArtifactBacking,
        range: Range<usize>,
    ) -> Result<(), ArtifactBuildError> {
        self.budget
            .reserve_source_backing(source_id, fingerprint, backing)?;
        self.reserve_image_backing(backing, ImageBackingClass::Source)?;
        let length = usize_to_u64(range.len(), "source_range_bytes")?;
        self.reserve_dependency(source_id, fingerprint, length)?;

        let logical_start = self.logical_len;
        let coalesced_range = self.source_tail.as_ref().and_then(|tail| {
            (tail.source_id == source_id
                && tail.fingerprint == fingerprint
                && tail.backing.identity() == backing.identity()
                && tail.range.end == range.start)
                .then_some(tail.range.start..range.end)
        });
        if let Some(combined_range) = coalesced_range {
            let previous_bytes = usize_to_u64(
                range.start - combined_range.start,
                "coalesced_source_range_bytes",
            )?;
            let replacement =
                backing.segment(logical_start - previous_bytes, combined_range.clone())?;
            self.reserve_output(length)?;
            let last =
                self.segments
                    .last_mut()
                    .ok_or(ArtifactBuildError::InternalSegmentState {
                        message: "source tail exists without a retained segment",
                    })?;
            *last = replacement;
            self.source_tail = Some(SourceTail {
                source_id,
                fingerprint,
                backing: backing.clone(),
                range: combined_range,
            });
        } else {
            let segment = backing.segment(logical_start, range.clone())?;
            self.reserve_output(length)?;
            self.reserve_segment_capacity(1, 1)?;
            self.segments.push(segment);
            self.segment_usage.push(ArtifactSegmentUsage {
                backing: backing.clone(),
                provenance: ArtifactSegmentProvenance::Source {
                    source_id,
                    fingerprint,
                },
            });
            self.source_tail = Some(SourceTail {
                source_id,
                fingerprint,
                backing: backing.clone(),
                range,
            });
        }
        self.referenced_source_bytes = checked_add(
            self.referenced_source_bytes,
            length,
            "referenced_source_bytes",
        )?;
        self.source_ranges = checked_add(self.source_ranges, 1, "source_ranges")?;
        Ok(())
    }

    fn reserve_dependency(
        &mut self,
        source_id: SourceId,
        fingerprint: SourceFingerprint,
        referenced_bytes: u64,
    ) -> Result<(), ArtifactBuildError> {
        match self.dependency_indices.get(&source_id).copied() {
            Some(index) => {
                let dependency = &mut self.dependencies[index];
                if dependency.fingerprint != fingerprint {
                    return Err(ArtifactBuildError::ConflictingSourceFingerprint {
                        source_id,
                        first: dependency.fingerprint.digest(),
                        second: fingerprint.digest(),
                    });
                }
                dependency.referenced_bytes = checked_add(
                    dependency.referenced_bytes,
                    referenced_bytes,
                    "dependency_referenced_bytes",
                )?;
            }
            None => {
                self.dependency_indices
                    .reserve_for_insert("artifact dependency index")?;
                self.budget.grow_retained_vec(
                    &mut self.dependencies,
                    1,
                    "artifact source dependencies",
                )?;
                let index = self.dependencies.len();
                self.dependencies.push(ArtifactSourceDependency {
                    source: source_id,
                    fingerprint,
                    referenced_bytes,
                });
                let previous = self.dependency_indices.insert_reserved(source_id, index);
                debug_assert!(previous.is_none());
            }
        }
        Ok(())
    }

    fn push_generated(&mut self, backing: &ArtifactBacking) -> Result<(), ArtifactBuildError> {
        self.budget.reserve_generated_backing(backing)?;
        self.reserve_image_backing(backing, ImageBackingClass::Generated)?;
        let length = usize_to_u64(backing.len(), "generated_chunk_bytes")?;
        let segment = backing.segment(self.logical_len, 0..backing.len())?;
        self.reserve_output(length)?;
        self.reserve_segment_capacity(1, 1)?;
        self.generated_chunks = checked_add(self.generated_chunks, 1, "generated_chunks")?;
        self.segments.push(segment);
        self.segment_usage.push(ArtifactSegmentUsage {
            backing: backing.clone(),
            provenance: ArtifactSegmentProvenance::Generated,
        });
        self.source_tail = None;
        Ok(())
    }

    fn reserve_output(&mut self, length: u64) -> Result<(), ArtifactBuildError> {
        self.budget.reserve_proof_bytes(length)?;
        self.logical_len = checked_add(self.logical_len, length, "artifact_proof_bytes")?;
        Ok(())
    }

    fn reserve_segment_capacity(
        &mut self,
        additional: usize,
        additional_u64: u64,
    ) -> Result<(), ArtifactBuildError> {
        self.budget.reserve_segments(additional_u64)?;
        self.budget
            .grow_retained_vec(&mut self.segments, additional, "artifact byte segments")?;
        self.budget.grow_retained_vec(
            &mut self.segment_usage,
            additional,
            "artifact segment usage",
        )?;
        Ok(())
    }

    fn reserve_image_backing(
        &mut self,
        backing: &ArtifactBacking,
        incoming_class: ImageBackingClass,
    ) -> Result<(), ArtifactBuildError> {
        let identity = backing.identity();
        let allocation_bytes = backing
            .allocation_bytes()
            .map_err(ArtifactBudgetError::from)?;
        let existing = self
            .backings
            .get(&identity)
            .map(|usage| (usage.class, usage.allocation_bytes));

        let mut generated = self.owned_generated_bytes;
        let mut pinned = self.pinned_source_bytes;
        match (existing, incoming_class) {
            (Some((ImageBackingClass::Generated, existing_bytes)), ImageBackingClass::Source) => {
                generated = generated.checked_sub(existing_bytes).ok_or(
                    ArtifactBuildError::ArithmeticOverflow {
                        resource: "owned_generated_bytes",
                    },
                )?;
                pinned = checked_add(pinned, existing_bytes, "artifact_pinned_source_bytes")?;
            }
            (Some(_), _) => return Ok(()),
            (None, ImageBackingClass::Generated) => {
                generated = checked_add(generated, allocation_bytes, "owned_generated_bytes")?;
            }
            (None, ImageBackingClass::Source) => {
                pinned = checked_add(pinned, allocation_bytes, "artifact_pinned_source_bytes")?;
            }
        }

        match self.backings.get_mut(&identity) {
            Some(usage) => usage.class = ImageBackingClass::Source,
            None => {
                self.backings.reserve_for_insert("artifact backing index")?;
                let previous = self.backings.insert_reserved(
                    identity,
                    ImageBackingUsage {
                        class: incoming_class,
                        allocation_bytes,
                    },
                );
                debug_assert!(previous.is_none());
            }
        }
        self.owned_generated_bytes = generated;
        self.pinned_source_bytes = pinned;
        Ok(())
    }

    fn record_digest(
        &mut self,
        payload: &ArtifactPayload,
        range: Range<usize>,
        is_full_payload: bool,
    ) -> Result<(), ArtifactBuildError> {
        let reusable_digest = if is_full_payload
            && matches!(
                payload.provenance(),
                ArtifactPayloadProvenance::Source { .. }
            ) {
            payload.digest()
        } else {
            None
        };
        if let Some(digest) = reusable_digest
            && self.digest_hint.is_none()
            && self.digest_builder.consumed_bytes() == 0
        {
            self.digest_hint = Some(DeferredDigestHint {
                backing: payload.backing().clone(),
                range,
                digest,
            });
            return Ok(());
        }
        self.flush_deferred_digest()?;
        self.digest_builder
            .update(&payload.backing().as_slice()[range])?;
        Ok(())
    }

    fn flush_deferred_digest(&mut self) -> Result<(), ArtifactBuildError> {
        if let Some(deferred) = self.digest_hint.take() {
            self.digest_builder
                .update(&deferred.backing.as_slice()[deferred.range])?;
        }
        Ok(())
    }

    pub(crate) fn seal(mut self) -> Result<SealedImage, ArtifactBuildError> {
        if self.logical_len != self.declared_output_len {
            return Err(ArtifactBuildError::InternalLengthMismatch {
                declared: self.declared_output_len,
                actual: self.logical_len,
            });
        }
        if self.segments.len() != self.segment_usage.len() {
            return Err(ArtifactBuildError::InternalInvariant {
                message: "artifact byte segments and usage metadata diverged",
            });
        }
        self.dependencies
            .sort_unstable_by_key(|dependency| dependency.source);
        let segment_count = usize_to_u64(self.segments.len(), "segment_count")?;
        let segment_metadata = vec_allocation_bytes::<ByteSegment>(self.segments.capacity())
            .map_err(ArtifactBudgetError::from)?;
        let dependency_metadata =
            vec_allocation_bytes::<ArtifactSourceDependency>(self.dependencies.capacity())
                .map_err(ArtifactBudgetError::from)?;
        let usage_metadata =
            vec_allocation_bytes::<ArtifactSegmentUsage>(self.segment_usage.capacity())
                .map_err(ArtifactBudgetError::from)?;
        let metadata_bytes = checked_add(
            checked_add(
                segment_metadata,
                dependency_metadata,
                "artifact_metadata_bytes",
            )?,
            usage_metadata,
            "artifact_metadata_bytes",
        )?;

        let (digest, digest_passes, digest_reuses) = match self.digest_hint.take() {
            Some(hint) if self.digest_builder.consumed_bytes() == 0 => (hint.digest, 0, 1),
            Some(_) => {
                return Err(ArtifactBuildError::InternalInvariant {
                    message: "a deferred digest hint remained after digest bytes were consumed",
                });
            }
            None => (self.digest_builder.finalize()?, 1, 0),
        };

        let image = SegmentedBytes::new(self.segments)?;
        if image.len() != self.logical_len {
            return Err(ArtifactBuildError::InternalLengthMismatch {
                declared: self.logical_len,
                actual: image.len(),
            });
        }

        let retained_bytes = checked_add(
            checked_add(
                self.owned_generated_bytes,
                self.pinned_source_bytes,
                "artifact_retained_bytes",
            )?,
            metadata_bytes,
            "artifact_retained_bytes",
        )?;

        Ok(SealedImage {
            image,
            segment_usage: self.segment_usage,
            digest,
            dependencies: self.dependencies,
            footprint: ArtifactFootprint {
                proof_bytes: self.logical_len,
                retained_bytes,
                referenced_source_bytes: self.referenced_source_bytes,
                generated_bytes: self.owned_generated_bytes,
                metadata_bytes,
                pinned_source_bytes: self.pinned_source_bytes,
                inspection_bytes: 0,
                segments: segment_count,
            },
            counters: ArtifactBuildCounters {
                source_ranges: self.source_ranges,
                generated_chunks: self.generated_chunks,
                digest_passes,
                digest_reuses,
                validation_passes: 0,
            },
        })
    }
}

pub(crate) struct SealedImage {
    image: SegmentedBytes,
    segment_usage: Vec<ArtifactSegmentUsage>,
    digest: DigestV1,
    dependencies: Vec<ArtifactSourceDependency>,
    footprint: ArtifactFootprint,
    counters: ArtifactBuildCounters,
}

impl SealedImage {
    pub(crate) const fn segmented(&self) -> &SegmentedBytes {
        &self.image
    }

    pub(crate) const fn digest(&self) -> DigestV1 {
        self.digest
    }

    pub(crate) fn dependencies(&self) -> &[ArtifactSourceDependency] {
        &self.dependencies
    }

    pub(crate) fn into_validated(
        mut self,
        inspection_bytes: u64,
    ) -> Result<ValidatedImage, ArtifactBuildError> {
        self.footprint.inspection_bytes = inspection_bytes;
        self.footprint.metadata_bytes = checked_add(
            self.footprint.metadata_bytes,
            inspection_bytes,
            "artifact_metadata_bytes",
        )?;
        self.footprint.retained_bytes = checked_add(
            self.footprint.retained_bytes,
            inspection_bytes,
            "artifact_retained_bytes",
        )?;
        self.counters.validation_passes = 1;
        Ok(ValidatedImage {
            image: self.image,
            segment_usage: self.segment_usage,
            digest: self.digest,
            dependencies: self.dependencies,
            footprint: self.footprint,
            counters: self.counters,
        })
    }
}

pub(crate) struct ValidatedImage {
    image: SegmentedBytes,
    segment_usage: Vec<ArtifactSegmentUsage>,
    digest: DigestV1,
    dependencies: Vec<ArtifactSourceDependency>,
    footprint: ArtifactFootprint,
    counters: ArtifactBuildCounters,
}

impl ValidatedImage {
    pub(crate) const fn len(&self) -> u64 {
        self.footprint.proof_bytes
    }

    pub(crate) const fn digest(&self) -> DigestV1 {
        self.digest
    }

    pub(crate) fn dependencies(&self) -> &[ArtifactSourceDependency] {
        &self.dependencies
    }

    pub(crate) const fn footprint(&self) -> ArtifactFootprint {
        self.footprint
    }

    pub(crate) const fn counters(&self) -> ArtifactBuildCounters {
        self.counters
    }

    pub(crate) fn reader(&self) -> ArtifactReader<'_> {
        ArtifactReader {
            image: &self.image,
            position: 0,
            segment_index: 0,
        }
    }

    pub(crate) fn contiguous_range(&self, range: Range<u64>) -> Option<&[u8]> {
        self.image.contiguous_range(range)
    }

    pub(crate) fn stream_verified_to(
        &self,
        sink: &mut impl Write,
    ) -> Result<ArtifactStreamReceipt, ArtifactStreamError> {
        let mut digest = DigestV1Builder::new(self.image.len());
        for segment in self.image.segments() {
            sink.write_all(segment.as_slice())?;
            digest.update(segment.as_slice())?;
        }
        let actual = digest.finalize()?;
        if actual != self.digest {
            return Err(ArtifactStreamError::DigestMismatch {
                expected: self.digest,
                actual,
            });
        }
        Ok(ArtifactStreamReceipt {
            bytes_written: self.image.len(),
            digest: actual,
        })
    }
}

/// An independent cursor over an immutable prepared artifact.
pub struct ArtifactReader<'artifact> {
    image: &'artifact SegmentedBytes,
    position: u64,
    segment_index: usize,
}

impl Read for ArtifactReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.position == self.image.len() {
            return Ok(0);
        }
        let available = self.image.len() - self.position;
        let requested = available.min(usize_to_u64_io(output.len())?);
        let mut remaining = usize::try_from(requested)
            .map_err(|_| invalid_data("artifact read length does not fit usize"))?;
        let mut written = 0_usize;

        while remaining != 0 {
            let segment = self
                .image
                .segments()
                .get(self.segment_index)
                .ok_or_else(|| invalid_data("artifact segment coverage ended during read"))?;
            let logical = segment.logical_range();
            if self.position < logical.start || self.position >= logical.end {
                return Err(invalid_data("artifact segment coverage contains a gap"));
            }
            let segment_offset = usize::try_from(self.position - logical.start)
                .map_err(|_| invalid_data("artifact segment offset does not fit usize"))?;
            let bytes = segment.as_slice();
            let take = remaining.min(bytes.len() - segment_offset);
            output[written..written + take]
                .copy_from_slice(&bytes[segment_offset..segment_offset + take]);
            written += take;
            remaining -= take;
            self.position = self
                .position
                .checked_add(usize_to_u64_io(take)?)
                .ok_or_else(|| invalid_data("artifact read position overflow"))?;
            if self.position == logical.end {
                self.segment_index += 1;
            }
        }
        Ok(written)
    }
}

impl Seek for ArtifactReader<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let base = match position {
            SeekFrom::Start(offset) => {
                if offset > self.image.len() {
                    return Err(invalid_input("artifact seek exceeds image length"));
                }
                self.position = offset;
                self.segment_index = segment_index_at(self.image, offset);
                return Ok(offset);
            }
            SeekFrom::End(_) => self.image.len(),
            SeekFrom::Current(_) => self.position,
        };
        let delta = match position {
            SeekFrom::End(delta) | SeekFrom::Current(delta) => delta,
            SeekFrom::Start(_) => unreachable!(),
        };
        let target = i128::from(base) + i128::from(delta);
        if target < 0 || target > i128::from(self.image.len()) {
            return Err(invalid_input("artifact seek is outside the image"));
        }
        self.position = u64::try_from(target)
            .map_err(|_| invalid_input("artifact seek target does not fit u64"))?;
        self.segment_index = segment_index_at(self.image, self.position);
        Ok(self.position)
    }
}

fn validate_backing_range(
    backing_len: usize,
    range: &Range<usize>,
) -> Result<(), ArtifactBuildError> {
    if range.start > range.end || range.end > backing_len {
        return Err(ArtifactBuildError::InvalidBackingRange {
            start: range.start,
            end: range.end,
            backing_len,
        });
    }
    Ok(())
}

fn checked_add(left: u64, right: u64, resource: &'static str) -> Result<u64, ArtifactBuildError> {
    left.checked_add(right)
        .ok_or(ArtifactBuildError::ArithmeticOverflow { resource })
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, ArtifactBuildError> {
    u64::try_from(value).map_err(|_| ArtifactBuildError::ArithmeticOverflow { resource })
}

fn usize_to_u64_io(value: usize) -> io::Result<u64> {
    u64::try_from(value).map_err(|_| invalid_data("artifact byte count does not fit u64"))
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn segment_index_at(image: &SegmentedBytes, position: u64) -> usize {
    image
        .segments()
        .partition_point(|segment| segment.logical_range().end <= position)
}

#[cfg(test)]
mod tests {
    use unity_asset_core::{SourceKind, VerifiedSourceImage, WorkspaceId};

    use super::*;
    use crate::artifact::payload::GeneratedChunkWriter;
    use crate::artifact::{ArtifactBudget, ArtifactBudgetUsage, ArtifactLimits};

    fn source_id() -> SourceId {
        SourceId::new(
            WorkspaceId::from_u128(13).unwrap(),
            SourceKind::SerializedFile,
            5,
        )
        .unwrap()
    }

    fn source_payload(bytes: &'static [u8]) -> ArtifactPayload {
        let verified = VerifiedSourceImage::verify(SourceKind::SerializedFile, Arc::from(bytes));
        ArtifactPayload::source_backed(source_id(), verified).unwrap()
    }

    fn generated_payload(
        transaction: &mut ArtifactBudgetTransaction<'_>,
        bytes: &[u8],
    ) -> ArtifactPayload {
        let mut writer = GeneratedChunkWriter::new(transaction);
        writer.extend_from_slice(bytes).unwrap();
        writer.finish(transaction).unwrap()
    }

    #[test]
    fn single_nonempty_full_payload_reuses_verified_digest() {
        let payload = source_payload(b"verified source");
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut transaction = budget.transaction();
        let mut builder = ImageBuilder::new(&mut transaction, payload.len()).unwrap();

        builder.push_payload_full(&payload).unwrap();
        let image = builder.seal().unwrap();

        assert_eq!(Some(image.digest), payload.digest());
        assert_eq!(image.counters.digest_passes(), 0);
        assert_eq!(image.counters.digest_reuses(), 1);
    }

    #[test]
    fn repeated_generated_backing_is_owned_once_and_uses_one_coverage_pass() {
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut transaction = budget.transaction();
        let payload = generated_payload(&mut transaction, b"chunk");
        let allocation = payload.backing().allocation_bytes().unwrap();
        let mut builder = ImageBuilder::new(&mut transaction, payload.len() * 2).unwrap();

        builder.push_payload_full(&payload).unwrap();
        builder.push_payload_full(&payload).unwrap();
        let image = builder.seal().unwrap();

        assert_eq!(image.digest, DigestV1::hash_bytes(b"chunkchunk"));
        assert_eq!(image.footprint.generated_bytes(), allocation);
        assert_eq!(image.counters.generated_chunks(), 2);
        assert_eq!(image.counters.digest_passes(), 1);
        assert_eq!(image.counters.digest_reuses(), 0);
    }

    #[test]
    fn single_generated_payload_is_hashed_exactly_once() {
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut transaction = budget.transaction();
        let payload = generated_payload(&mut transaction, b"generated");
        let mut builder = ImageBuilder::new(&mut transaction, payload.len()).unwrap();

        builder.push_payload_full(&payload).unwrap();
        let image = builder.seal().unwrap();

        assert_eq!(image.digest, DigestV1::hash_bytes(b"generated"));
        assert_eq!(image.counters.digest_passes(), 1);
        assert_eq!(image.counters.digest_reuses(), 0);
    }

    #[test]
    fn partial_source_range_is_hashed_during_push() {
        let payload = source_payload(b"abcdef");
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut transaction = budget.transaction();
        let mut builder = ImageBuilder::new(&mut transaction, 3).unwrap();

        builder.push_payload_range(&payload, 1..4).unwrap();
        let image = builder.seal().unwrap();

        assert_eq!(image.digest, DigestV1::hash_bytes(b"bcd"));
        assert_eq!(image.counters.digest_passes(), 1);
        assert_eq!(image.counters.digest_reuses(), 0);
    }

    #[test]
    fn empty_source_image_reuses_its_verified_digest_and_records_identity() {
        let payload = source_payload(b"");
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut transaction = budget.transaction();
        let mut builder = ImageBuilder::new(&mut transaction, 0).unwrap();

        builder.push_payload_full(&payload).unwrap();

        let image = builder.seal().unwrap();

        assert_eq!(image.digest, DigestV1::hash_bytes(b""));
        assert_eq!(image.dependencies.len(), 1);
        assert_eq!(image.dependencies[0].referenced_bytes(), 0);
        assert_eq!(image.footprint.pinned_source_bytes(), 0);
        assert_eq!(image.counters.digest_passes(), 0);
        assert_eq!(image.counters.digest_reuses(), 1);
    }

    #[test]
    fn constant_empty_image_uses_one_coverage_pass_without_source_identity() {
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut transaction = budget.transaction();
        let builder = ImageBuilder::new(&mut transaction, 0).unwrap();

        let image = builder.seal().unwrap();

        assert_eq!(image.digest, DigestV1::hash_bytes(b""));
        assert!(image.dependencies.is_empty());
        assert_eq!(image.counters.digest_passes(), 1);
        assert_eq!(image.counters.digest_reuses(), 0);
    }

    #[test]
    fn declared_length_failure_does_not_commit_transaction_usage() {
        let payload = source_payload(b"bytes");
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        {
            let mut transaction = budget.transaction();
            let mut builder = ImageBuilder::new(&mut transaction, payload.len() + 1).unwrap();
            builder.push_payload_full(&payload).unwrap();

            assert!(matches!(
                builder.seal(),
                Err(ArtifactBuildError::InternalLengthMismatch { .. })
            ));
        }

        assert_eq!(budget.committed_usage(), ArtifactBudgetUsage::default());
        assert_eq!(budget.live_scratch_bytes(), 0);
    }
}
