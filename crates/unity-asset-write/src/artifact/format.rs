use unity_asset_binary::SegmentedBytes;
use unity_asset_binary::asset::{SerializedFileInspection, SerializedFileParser};
use unity_asset_binary::bundle::{BundleInspection, BundleParser};
use unity_asset_binary::webfile::{WebFile, WebFileInspection};
use unity_asset_core::{
    AssetLoadBudget, DigestV1, DigestV1Builder, SourceFingerprint, SourceId, vec_allocation_bytes,
};
use yaml_rust2::parser::{Event, Parser};

use super::{ArtifactBudgetError, ArtifactBuildError, ArtifactSourceDependency};

const RESOURCE_LAYOUT_DOMAIN: &[u8] = b"unity-asset:resource-layout:v1\0";
// yaml-rust2 owns scanner stacks, token strings, and anchor/tag tables. Charge a conservative
// linear allowance plus small-input overhead before entering its infallible allocation paths.
const YAML_PARSER_WORK_MULTIPLIER: u64 = 32;
const YAML_PARSER_FIXED_WORK_BYTES: u64 = 8 * 1024;

/// The independently inspected wire family of a prepared artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PreparedArtifactKind {
    SerializedFile,
    AssetBundle,
    WebFile,
    StreamedResource,
    Yaml,
    VerbatimSource,
}

/// Opaque parser proof retained with one exact byte image.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PreparedArtifactFormat {
    SerializedFile(SerializedFileInspection),
    AssetBundle(BundleInspection),
    WebFile(WebFileInspection),
    StreamedResource(StreamedResourceInspection),
    Yaml(YamlInspection),
    VerbatimSource(VerbatimSourceInspection),
}

impl PreparedArtifactFormat {
    #[must_use]
    pub const fn kind(&self) -> PreparedArtifactKind {
        match self {
            Self::SerializedFile(_) => PreparedArtifactKind::SerializedFile,
            Self::AssetBundle(_) => PreparedArtifactKind::AssetBundle,
            Self::WebFile(_) => PreparedArtifactKind::WebFile,
            Self::StreamedResource(_) => PreparedArtifactKind::StreamedResource,
            Self::Yaml(_) => PreparedArtifactKind::Yaml,
            Self::VerbatimSource(_) => PreparedArtifactKind::VerbatimSource,
        }
    }

    pub(crate) fn inspect(
        expected: ExpectedArtifactFormat,
        image: &SegmentedBytes,
        image_digest: DigestV1,
        source_dependencies: &[ArtifactSourceDependency],
        budget: &mut AssetLoadBudget,
    ) -> Result<(Self, u64), ArtifactBuildError> {
        match expected {
            ExpectedArtifactFormat::SerializedFile => Ok((
                Self::SerializedFile(SerializedFileParser::validate_segmented_with_budget(
                    image, budget,
                )?),
                0,
            )),
            ExpectedArtifactFormat::AssetBundle => Ok((
                Self::AssetBundle(BundleParser::inspect_segmented_with_budget(image, budget)?),
                0,
            )),
            ExpectedArtifactFormat::WebFile => Ok((
                Self::WebFile(WebFile::inspect_segmented_with_budget(image, budget)?),
                0,
            )),
            ExpectedArtifactFormat::StreamedResource(layout) => {
                let preaccounted_heap_bytes = layout.inspection_metadata_precharged();
                Ok((
                    Self::StreamedResource(layout.inspect_image(image, budget)?),
                    preaccounted_heap_bytes,
                ))
            }
            ExpectedArtifactFormat::Yaml => {
                Ok((Self::Yaml(YamlInspection::inspect(image, budget)?), 0))
            }
            ExpectedArtifactFormat::VerbatimSource(proof) => {
                proof.validate(image, image_digest, source_dependencies)?;
                Ok((Self::VerbatimSource(proof), 0))
            }
        }
    }

    pub(crate) fn retained_heap_bytes(&self) -> Result<u64, ArtifactBuildError> {
        match self {
            Self::SerializedFile(proof) => Ok(proof.retained_heap_bytes()?),
            Self::AssetBundle(proof) => Ok(proof.retained_heap_bytes()?),
            Self::WebFile(proof) => Ok(proof.retained_heap_bytes()?),
            Self::StreamedResource(proof) => proof.retained_heap_bytes(),
            Self::Yaml(_) | Self::VerbatimSource(_) => Ok(0),
        }
    }
}

/// Syntax proof produced by reparsing the exact segmented YAML image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YamlInspection {
    encoded_bytes: u64,
    documents: u64,
    events: u64,
    max_depth: u32,
}

impl YamlInspection {
    fn inspect(
        image: &SegmentedBytes,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ArtifactBuildError> {
        let parser_work = image
            .len()
            .checked_mul(YAML_PARSER_WORK_MULTIPLIER)
            .and_then(|bytes| bytes.checked_add(YAML_PARSER_FIXED_WORK_BYTES))
            .ok_or(ArtifactBuildError::ArithmeticOverflow {
                resource: "yaml_parser_work_bytes",
            })?;
        let charged_bytes =
            image
                .len()
                .checked_add(parser_work)
                .ok_or(ArtifactBuildError::ArithmeticOverflow {
                    resource: "yaml_parser_work_bytes",
                })?;
        budget.consume_bytes(charged_bytes)?;
        validate_segmented_utf8(image)?;

        let mut parser = Parser::new(SegmentedChars::new(image)).keep_tags(true);
        let mut documents = 0_u64;
        let mut events = 0_u64;
        let mut depth = 0_u32;
        let mut max_depth = 0_u32;
        loop {
            let (event, _) = parser
                .next_token()
                .map_err(ArtifactBuildError::InvalidYaml)?;
            budget.consume_entries(1)?;
            events = events
                .checked_add(1)
                .ok_or(ArtifactBuildError::ArithmeticOverflow {
                    resource: "yaml_event_count",
                })?;
            match event {
                Event::DocumentStart => {
                    documents =
                        documents
                            .checked_add(1)
                            .ok_or(ArtifactBuildError::ArithmeticOverflow {
                                resource: "yaml_document_count",
                            })?;
                }
                Event::SequenceStart(..) | Event::MappingStart(..) => {
                    depth = depth
                        .checked_add(1)
                        .ok_or(ArtifactBuildError::ArithmeticOverflow {
                            resource: "yaml_depth",
                        })?;
                    budget.observe_depth(depth)?;
                    max_depth = max_depth.max(depth);
                }
                Event::SequenceEnd | Event::MappingEnd => {
                    depth = depth
                        .checked_sub(1)
                        .ok_or(ArtifactBuildError::InternalInvariant {
                            message: "YAML parser emitted an unmatched collection end",
                        })?;
                }
                Event::StreamEnd => break,
                Event::Nothing => {
                    return Err(ArtifactBuildError::InternalInvariant {
                        message: "YAML parser emitted its reserved Nothing event",
                    });
                }
                Event::StreamStart | Event::DocumentEnd | Event::Alias(_) | Event::Scalar(..) => {}
            }
        }
        if depth != 0 {
            return Err(ArtifactBuildError::InternalInvariant {
                message: "YAML parser ended with open collections",
            });
        }
        Ok(Self {
            encoded_bytes: image.len(),
            documents,
            events,
            max_depth,
        })
    }

    #[must_use]
    pub const fn encoded_bytes(&self) -> u64 {
        self.encoded_bytes
    }

    #[must_use]
    pub const fn documents(&self) -> u64 {
        self.documents
    }

    #[must_use]
    pub const fn events(&self) -> u64 {
        self.events
    }

    #[must_use]
    pub const fn max_depth(&self) -> u32 {
        self.max_depth
    }
}

/// Identity proof for an unchanged artifact that retains its complete verified source image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerbatimSourceInspection {
    source_id: SourceId,
    fingerprint: SourceFingerprint,
    length: u64,
}

impl VerbatimSourceInspection {
    pub(crate) const fn new(
        source_id: SourceId,
        fingerprint: SourceFingerprint,
        length: u64,
    ) -> Self {
        Self {
            source_id,
            fingerprint,
            length,
        }
    }

    fn validate(
        self,
        image: &SegmentedBytes,
        image_digest: DigestV1,
        dependencies: &[ArtifactSourceDependency],
    ) -> Result<(), ArtifactBuildError> {
        if image.len() != self.length {
            return Err(ArtifactBuildError::VerbatimSourceLengthMismatch {
                expected: self.length,
                actual: image.len(),
            });
        }
        if image_digest != self.fingerprint.digest() {
            return Err(ArtifactBuildError::VerbatimSourceDigestMismatch {
                expected: self.fingerprint.digest(),
                actual: image_digest,
            });
        }
        let [dependency] = dependencies else {
            return Err(ArtifactBuildError::VerbatimSourceProvenanceMismatch);
        };
        if dependency.source() != self.source_id
            || dependency.fingerprint() != self.fingerprint
            || dependency.referenced_bytes() != self.length
        {
            return Err(ArtifactBuildError::VerbatimSourceProvenanceMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    #[must_use]
    pub const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }

    #[must_use]
    pub const fn length(&self) -> u64 {
        self.length
    }
}

struct SegmentedByteCursor<'image> {
    image: &'image SegmentedBytes,
    segment: usize,
    byte: usize,
    position: u64,
}

impl<'image> SegmentedByteCursor<'image> {
    const fn new(image: &'image SegmentedBytes) -> Self {
        Self {
            image,
            segment: 0,
            byte: 0,
            position: 0,
        }
    }

    fn next_byte(&mut self) -> Option<u8> {
        loop {
            let segment = self.image.segments().get(self.segment)?;
            if let Some(value) = segment.as_slice().get(self.byte).copied() {
                self.byte += 1;
                self.position += 1;
                return Some(value);
            }
            self.segment += 1;
            self.byte = 0;
        }
    }

    fn next_char(&mut self) -> Result<Option<char>, u64> {
        let start = self.position;
        let Some(first) = self.next_byte() else {
            return Ok(None);
        };
        let width = match first {
            0x00..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(start),
        };
        let mut encoded = [0_u8; 4];
        encoded[0] = first;
        for slot in &mut encoded[1..width] {
            *slot = self.next_byte().ok_or(start)?;
        }
        let encoded = std::str::from_utf8(&encoded[..width]).map_err(|_| start)?;
        encoded.chars().next().map(Some).ok_or(start)
    }
}

struct SegmentedChars<'image> {
    bytes: SegmentedByteCursor<'image>,
}

impl<'image> SegmentedChars<'image> {
    const fn new(image: &'image SegmentedBytes) -> Self {
        Self {
            bytes: SegmentedByteCursor::new(image),
        }
    }
}

impl Iterator for SegmentedChars<'_> {
    type Item = char;

    fn next(&mut self) -> Option<Self::Item> {
        match self.bytes.next_char() {
            Ok(value) => value,
            Err(_) => {
                debug_assert!(
                    false,
                    "segmented YAML was validated before character iteration"
                );
                None
            }
        }
    }
}

fn validate_segmented_utf8(image: &SegmentedBytes) -> Result<(), ArtifactBuildError> {
    let mut cursor = SegmentedByteCursor::new(image);
    while cursor
        .next_char()
        .map_err(|offset| ArtifactBuildError::InvalidYamlUtf8 { offset })?
        .is_some()
    {}
    Ok(())
}

/// Digest of canonical resource extents, independent from artifact content identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceLayoutDigest(DigestV1);

impl ResourceLayoutDigest {
    #[must_use]
    pub const fn digest(self) -> DigestV1 {
        self.0
    }
}

/// One payload extent retained by an opaque streamed-resource layout proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamedResourceExtentInspection {
    payload_digest: DigestV1,
    offset: u64,
    length: u64,
    alignment: u32,
    padding_before: u64,
}

impl StreamedResourceExtentInspection {
    pub(crate) const fn new(
        payload_digest: DigestV1,
        offset: u64,
        length: u64,
        alignment: u32,
    ) -> Self {
        Self {
            payload_digest,
            offset,
            length,
            alignment,
            padding_before: 0,
        }
    }

    #[must_use]
    pub const fn payload_digest(self) -> DigestV1 {
        self.payload_digest
    }

    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }

    #[must_use]
    pub const fn length(self) -> u64 {
        self.length
    }

    #[must_use]
    pub const fn alignment(self) -> u32 {
        self.alignment
    }

    #[must_use]
    pub const fn padding_before(self) -> u64 {
        self.padding_before
    }
}

/// Opaque proof that an image matches one ordered resource allocation layout.
#[derive(Debug, PartialEq, Eq)]
pub struct StreamedResourceInspection {
    length: u64,
    extents: Vec<StreamedResourceExtentInspection>,
    payload_bytes: u64,
    padding_bytes: u64,
    layout_digest: ResourceLayoutDigest,
}

impl StreamedResourceInspection {
    #[must_use]
    pub const fn length(&self) -> u64 {
        self.length
    }

    #[must_use]
    pub fn extents(&self) -> &[StreamedResourceExtentInspection] {
        &self.extents
    }

    #[must_use]
    pub const fn payload_bytes(&self) -> u64 {
        self.payload_bytes
    }

    #[must_use]
    pub const fn padding_bytes(&self) -> u64 {
        self.padding_bytes
    }

    #[must_use]
    pub const fn layout_digest(&self) -> ResourceLayoutDigest {
        self.layout_digest
    }

    fn retained_heap_bytes(&self) -> Result<u64, ArtifactBuildError> {
        vec_allocation_bytes::<StreamedResourceExtentInspection>(self.extents.capacity())
            .map_err(ArtifactBudgetError::from)
            .map_err(Into::into)
    }
}

/// Validated resource-layout input produced by the resource allocation planner.
#[derive(Debug)]
pub(crate) struct StreamedResourceLayoutProof {
    inspection: StreamedResourceInspection,
    inspection_metadata_precharged: u64,
}

impl StreamedResourceLayoutProof {
    pub(super) fn from_builder_extents(
        mut extents: Vec<StreamedResourceExtentInspection>,
    ) -> Result<Self, ArtifactBuildError> {
        let mut cursor = 0_u64;
        let mut payload_bytes = 0_u64;
        let mut padding_bytes = 0_u64;
        for (ordinal, extent) in extents.iter_mut().enumerate() {
            if extent.alignment == 0 || !extent.alignment.is_power_of_two() {
                return Err(ArtifactBuildError::InvalidStreamedResourceAlignment {
                    alignment: extent.alignment,
                });
            }
            let expected_offset = if ordinal == 0 {
                0
            } else {
                align_up(cursor, extent.alignment)?
            };
            if extent.offset != expected_offset {
                return Err(ArtifactBuildError::StreamedResourceExtentOffsetMismatch {
                    ordinal,
                    expected: expected_offset,
                    actual: extent.offset,
                });
            }
            extent.padding_before = extent.offset - cursor;
            padding_bytes = checked_add(
                padding_bytes,
                extent.padding_before,
                "resource_layout_padding_bytes",
            )?;
            payload_bytes = checked_add(
                payload_bytes,
                extent.length,
                "resource_layout_payload_bytes",
            )?;
            cursor = checked_add(extent.offset, extent.length, "resource_layout_extent_end")?;
        }
        let layout_digest = resource_layout_digest(&extents)?;
        Ok(Self {
            inspection: StreamedResourceInspection {
                length: cursor,
                extents,
                payload_bytes,
                padding_bytes,
                layout_digest,
            },
            inspection_metadata_precharged: 0,
        })
    }

    #[cfg(test)]
    pub(crate) fn single_extent(
        payload_digest: DigestV1,
        length: u64,
        alignment: u32,
    ) -> Result<Self, ArtifactBuildError> {
        Self::from_builder_extents(vec![StreamedResourceExtentInspection::new(
            payload_digest,
            0,
            length,
            alignment,
        )])
    }

    pub(crate) const fn length(&self) -> u64 {
        self.inspection.length
    }

    pub(crate) fn retained_heap_bytes(&self) -> Result<u64, ArtifactBuildError> {
        self.inspection.retained_heap_bytes()
    }

    pub(crate) fn mark_inspection_metadata_precharged(
        &mut self,
        bytes: u64,
    ) -> Result<(), ArtifactBuildError> {
        if bytes != self.retained_heap_bytes()? {
            return Err(ArtifactBuildError::InternalInvariant {
                message: "precharged resource inspection metadata does not match its allocation",
            });
        }
        self.inspection_metadata_precharged = bytes;
        Ok(())
    }

    pub(crate) const fn inspection_metadata_precharged(&self) -> u64 {
        self.inspection_metadata_precharged
    }

    fn inspect_image(
        self,
        image: &SegmentedBytes,
        budget: &mut AssetLoadBudget,
    ) -> Result<StreamedResourceInspection, ArtifactBuildError> {
        self.inspect_image_with_observer(image, budget, &mut || {})
    }

    fn inspect_image_with_observer(
        self,
        image: &SegmentedBytes,
        budget: &mut AssetLoadBudget,
        observe_segment: &mut impl FnMut(),
    ) -> Result<StreamedResourceInspection, ArtifactBuildError> {
        if self.inspection.length != image.len() {
            return Err(ArtifactBuildError::StreamedResourceLengthMismatch {
                planned: self.inspection.length,
                actual: image.len(),
            });
        }
        let extent_count = u64::try_from(self.inspection.extents.len()).map_err(|_| {
            ArtifactBuildError::ArithmeticOverflow {
                resource: "resource_layout_extent_count",
            }
        })?;
        budget.consume_entries(extent_count)?;
        budget.consume_bytes(image.len())?;

        let mut cursor = ResourceSegmentCursor::new(image);
        for (ordinal, extent) in self.inspection.extents.iter().enumerate() {
            if !cursor.consume_zero_until(extent.offset, observe_segment)? {
                return Err(ArtifactBuildError::NonZeroStreamedResourcePadding { ordinal });
            }
            let end = checked_add(extent.offset, extent.length, "resource_layout_extent_end")?;
            let actual = cursor.digest_until(end, observe_segment)?;
            if actual != extent.payload_digest {
                return Err(ArtifactBuildError::StreamedResourcePayloadDigestMismatch {
                    ordinal,
                    expected: extent.payload_digest,
                    actual,
                });
            }
        }
        if cursor.position() != image.len() {
            return Err(ArtifactBuildError::StreamedResourceLengthMismatch {
                planned: cursor.position(),
                actual: image.len(),
            });
        }
        Ok(self.inspection)
    }

    #[cfg(test)]
    fn inspect_image_with_visit_count(
        self,
        image: &SegmentedBytes,
        budget: &mut AssetLoadBudget,
    ) -> Result<(StreamedResourceInspection, u64), ArtifactBuildError> {
        let mut visits = 0_u64;
        let inspection = self.inspect_image_with_observer(image, budget, &mut || visits += 1)?;
        Ok((inspection, visits))
    }
}

#[derive(Debug)]
pub(crate) enum ExpectedArtifactFormat {
    SerializedFile,
    AssetBundle,
    WebFile,
    StreamedResource(StreamedResourceLayoutProof),
    Yaml,
    VerbatimSource(VerbatimSourceInspection),
}

fn align_up(value: u64, alignment: u32) -> Result<u64, ArtifactBuildError> {
    let alignment = u64::from(alignment);
    let mask = alignment - 1;
    value.checked_add(mask).map(|value| value & !mask).ok_or(
        ArtifactBuildError::ArithmeticOverflow {
            resource: "resource_layout_alignment",
        },
    )
}

fn resource_layout_digest(
    extents: &[StreamedResourceExtentInspection],
) -> Result<ResourceLayoutDigest, ArtifactBuildError> {
    const EXTENT_BYTES: u64 = DigestV1::BYTE_LEN as u64 + 8 + 4 + 8;

    let extent_count =
        u64::try_from(extents.len()).map_err(|_| ArtifactBuildError::ArithmeticOverflow {
            resource: "resource_layout_extent_count",
        })?;
    let extent_bytes =
        extent_count
            .checked_mul(EXTENT_BYTES)
            .ok_or(ArtifactBuildError::ArithmeticOverflow {
                resource: "resource_layout_digest_length",
            })?;
    let domain_bytes = u64::try_from(RESOURCE_LAYOUT_DOMAIN.len()).map_err(|_| {
        ArtifactBuildError::ArithmeticOverflow {
            resource: "resource_layout_digest_length",
        }
    })?;
    let declared = domain_bytes
        .checked_add(8)
        .and_then(|bytes| bytes.checked_add(extent_bytes))
        .ok_or(ArtifactBuildError::ArithmeticOverflow {
            resource: "resource_layout_digest_length",
        })?;
    let mut digest = DigestV1Builder::new(declared);
    digest.update(RESOURCE_LAYOUT_DOMAIN)?;
    digest.update(&extent_count.to_le_bytes())?;
    for extent in extents {
        digest.update(extent.payload_digest.as_bytes())?;
        digest.update(&extent.length.to_le_bytes())?;
        digest.update(&extent.alignment.to_le_bytes())?;
        digest.update(&extent.offset.to_le_bytes())?;
    }
    Ok(ResourceLayoutDigest(digest.finalize()?))
}

struct ResourceSegmentCursor<'image> {
    image: &'image SegmentedBytes,
    segment_index: usize,
    position: u64,
}

impl<'image> ResourceSegmentCursor<'image> {
    const fn new(image: &'image SegmentedBytes) -> Self {
        Self {
            image,
            segment_index: 0,
            position: 0,
        }
    }

    const fn position(&self) -> u64 {
        self.position
    }

    fn consume_zero_until(
        &mut self,
        end: u64,
        observe_segment: &mut impl FnMut(),
    ) -> Result<bool, ArtifactBuildError> {
        let mut is_zero = true;
        self.visit_until(end, observe_segment, &mut |bytes| {
            is_zero &= bytes.iter().all(|byte| *byte == 0);
            Ok(())
        })?;
        Ok(is_zero)
    }

    fn digest_until(
        &mut self,
        end: u64,
        observe_segment: &mut impl FnMut(),
    ) -> Result<DigestV1, ArtifactBuildError> {
        let mut digest = DigestV1Builder::new(end.checked_sub(self.position).ok_or(
            ArtifactBuildError::InternalInvariant {
                message: "resource proof cursor moved past a payload extent",
            },
        )?);
        self.visit_until(end, observe_segment, &mut |bytes| {
            digest.update(bytes)?;
            Ok(())
        })?;
        Ok(digest.finalize()?)
    }

    fn visit_until(
        &mut self,
        end: u64,
        observe_segment: &mut impl FnMut(),
        visit: &mut impl FnMut(&[u8]) -> Result<(), ArtifactBuildError>,
    ) -> Result<(), ArtifactBuildError> {
        if end < self.position || end > self.image.len() {
            return Err(ArtifactBuildError::InternalInvariant {
                message: "resource proof cursor target exceeds the inspected image",
            });
        }
        while self.position < end {
            let segment = self.image.segments().get(self.segment_index).ok_or(
                ArtifactBuildError::InternalInvariant {
                    message: "resource proof segment coverage ended early",
                },
            )?;
            let logical = segment.logical_range();
            if self.position < logical.start || self.position >= logical.end {
                return Err(ArtifactBuildError::InternalInvariant {
                    message: "resource proof segment coverage is not contiguous",
                });
            }
            let chunk_end = end.min(logical.end);
            let start = usize::try_from(self.position - logical.start).map_err(|_| {
                ArtifactBuildError::ArithmeticOverflow {
                    resource: "resource_proof_segment_offset",
                }
            })?;
            let chunk_end = usize::try_from(chunk_end - logical.start).map_err(|_| {
                ArtifactBuildError::ArithmeticOverflow {
                    resource: "resource_proof_segment_offset",
                }
            })?;
            observe_segment();
            visit(&segment.as_slice()[start..chunk_end])?;
            self.position = checked_add(
                logical.start,
                u64::try_from(chunk_end).map_err(|_| ArtifactBuildError::ArithmeticOverflow {
                    resource: "resource_proof_segment_offset",
                })?,
                "resource_proof_segment_offset",
            )?;
            if self.position == logical.end {
                self.segment_index += 1;
            }
        }
        Ok(())
    }
}

fn checked_add(left: u64, right: u64, resource: &'static str) -> Result<u64, ArtifactBuildError> {
    left.checked_add(right)
        .ok_or(ArtifactBuildError::ArithmeticOverflow { resource })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use unity_asset_binary::ByteSegment;

    use super::*;

    #[test]
    fn empty_resource_image_proves_one_zero_length_extent() {
        let expected_digest = DigestV1::hash_bytes(b"");
        let layout = StreamedResourceLayoutProof::single_extent(expected_digest, 0, 16).unwrap();
        let image = SegmentedBytes::new(Vec::new()).unwrap();
        let mut budget = AssetLoadBudget::default();

        let proof = layout.inspect_image(&image, &mut budget).unwrap();

        assert_eq!(proof.length(), 0);
        assert_eq!(proof.payload_bytes(), 0);
        assert_eq!(proof.padding_bytes(), 0);
        assert_eq!(proof.extents().len(), 1);
        assert_eq!(proof.extents()[0].payload_digest(), expected_digest);
        assert_eq!(proof.extents()[0].offset(), 0);
        assert_eq!(proof.extents()[0].length(), 0);
    }

    #[test]
    fn resource_proof_rejects_one_extra_image_byte() {
        let layout =
            StreamedResourceLayoutProof::single_extent(DigestV1::hash_bytes(b"a"), 1, 1).unwrap();
        let image = SegmentedBytes::from_contiguous(Arc::<[u8]>::from(b"ab".as_slice())).unwrap();
        let mut budget = AssetLoadBudget::default();

        assert!(matches!(
            layout.inspect_image(&image, &mut budget),
            Err(ArtifactBuildError::StreamedResourceLengthMismatch {
                planned: 1,
                actual: 2
            })
        ));
    }

    #[test]
    fn resource_layout_digest_matches_the_canonical_ordered_encoding() {
        let first_digest = DigestV1::hash_bytes(b"first");
        let second_digest = DigestV1::hash_bytes(b"second");
        let extents = [
            StreamedResourceExtentInspection::new(first_digest, 0, 5, 1),
            StreamedResourceExtentInspection::new(second_digest, 8, 6, 8),
        ];
        let mut canonical = Vec::new();
        canonical.extend_from_slice(RESOURCE_LAYOUT_DOMAIN);
        canonical.extend_from_slice(&2_u64.to_le_bytes());
        canonical.extend_from_slice(first_digest.as_bytes());
        canonical.extend_from_slice(&5_u64.to_le_bytes());
        canonical.extend_from_slice(&1_u32.to_le_bytes());
        canonical.extend_from_slice(&0_u64.to_le_bytes());
        canonical.extend_from_slice(second_digest.as_bytes());
        canonical.extend_from_slice(&6_u64.to_le_bytes());
        canonical.extend_from_slice(&8_u32.to_le_bytes());
        canonical.extend_from_slice(&8_u64.to_le_bytes());

        assert_eq!(
            resource_layout_digest(&extents).unwrap().digest(),
            DigestV1::hash_bytes(&canonical)
        );
    }

    #[test]
    fn resource_proof_segment_visits_scale_linearly_with_extents_and_segments() {
        const COUNT: usize = 2_048;

        let byte = [0x5a_u8];
        let payload_digest = DigestV1::hash_bytes(&byte);
        let mut segments = Vec::with_capacity(COUNT);
        let mut extents = Vec::with_capacity(COUNT);
        for ordinal in 0..COUNT {
            let offset = u64::try_from(ordinal).unwrap();
            segments.push(ByteSegment::new(offset, Arc::<[u8]>::from(byte.as_slice())).unwrap());
            extents.push(StreamedResourceExtentInspection::new(
                payload_digest,
                offset,
                1,
                1,
            ));
        }
        let image = SegmentedBytes::new(segments).unwrap();
        let layout = StreamedResourceLayoutProof::from_builder_extents(extents).unwrap();
        let mut budget = AssetLoadBudget::default();

        let (inspection, visits) = layout
            .inspect_image_with_visit_count(&image, &mut budget)
            .unwrap();

        assert_eq!(inspection.extents().len(), COUNT);
        assert_eq!(visits, COUNT as u64);
    }
}
