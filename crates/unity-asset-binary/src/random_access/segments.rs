use std::ops::Range;
use std::sync::Arc;

use super::{allocation_error, validate_range};
use crate::error::{BinaryError, Result};

#[derive(Clone, Debug)]
enum ByteSegmentBacking {
    Slice(Arc<[u8]>),
    Vec(Arc<Vec<u8>>),
}

impl ByteSegmentBacking {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Slice(bytes) => bytes,
            Self::Vec(bytes) => bytes.as_slice(),
        }
    }
}

impl PartialEq for ByteSegmentBacking {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for ByteSegmentBacking {}

/// One immutable backing slice placed at an explicit logical byte offset.
///
/// This is an internal interoperability value. It is public only so higher-level
/// crates can pass segmented prepared artifacts into binary-owned entry points.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ByteSegment {
    logical_range: Range<u64>,
    backing: ByteSegmentBacking,
    backing_range: Range<usize>,
}

impl ByteSegment {
    /// Places an entire immutable allocation at `logical_start`.
    pub fn new(logical_start: u64, bytes: Arc<[u8]>) -> Result<Self> {
        let backing_end = bytes.len();
        Self::from_arc_range(logical_start, bytes, 0..backing_end)
    }

    /// Places a zero-copy range of an immutable allocation at `logical_start`.
    pub fn from_arc_range(
        logical_start: u64,
        bytes: Arc<[u8]>,
        backing_range: Range<usize>,
    ) -> Result<Self> {
        Self::from_backing_range(
            logical_start,
            ByteSegmentBacking::Slice(bytes),
            backing_range,
        )
    }

    /// Places an entire immutable generated allocation without converting its backing.
    pub fn from_arc_vec(logical_start: u64, bytes: Arc<Vec<u8>>) -> Result<Self> {
        let backing_end = bytes.len();
        Self::from_arc_vec_range(logical_start, bytes, 0..backing_end)
    }

    /// Places a zero-copy range of an immutable generated allocation.
    pub fn from_arc_vec_range(
        logical_start: u64,
        bytes: Arc<Vec<u8>>,
        backing_range: Range<usize>,
    ) -> Result<Self> {
        Self::from_backing_range(logical_start, ByteSegmentBacking::Vec(bytes), backing_range)
    }

    fn from_backing_range(
        logical_start: u64,
        backing: ByteSegmentBacking,
        backing_range: Range<usize>,
    ) -> Result<Self> {
        validate_backing_range(&backing_range, backing.as_slice().len())?;
        let segment_len = u64::try_from(backing_range.end - backing_range.start)
            .map_err(|_| BinaryError::invalid_data("byte segment length does not fit in u64"))?;
        let logical_end = logical_start.checked_add(segment_len).ok_or_else(|| {
            BinaryError::invalid_data(format!(
                "byte segment logical range overflows u64 at offset {logical_start} with length {segment_len}"
            ))
        })?;

        Ok(Self {
            logical_range: logical_start..logical_end,
            backing,
            backing_range,
        })
    }

    /// Returns the segment's half-open logical range.
    #[must_use]
    pub fn logical_range(&self) -> Range<u64> {
        self.logical_range.clone()
    }

    /// Returns the number of logical bytes in this segment.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.logical_range.end - self.logical_range.start
    }

    /// Returns whether this segment contains no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.logical_range.is_empty()
    }

    /// Returns the original immutable backing slice without copying it.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.backing.as_slice()[self.backing_range.clone()]
    }

    /// Repositions this complete segment without copying its immutable backing.
    pub fn rebase(&self, logical_start: u64) -> Result<Self> {
        Self::from_backing_range(
            logical_start,
            self.backing.clone(),
            self.backing_range.clone(),
        )
    }

    /// Repositions a contained logical range without copying its immutable backing.
    pub fn rebase_subrange(&self, source_range: Range<u64>, logical_start: u64) -> Result<Self> {
        if source_range.start > source_range.end
            || source_range.start < self.logical_start()
            || source_range.end > self.logical_end()
        {
            return Err(BinaryError::invalid_data(format!(
                "byte segment range {}..{} is outside logical range {}..{}",
                source_range.start,
                source_range.end,
                self.logical_start(),
                self.logical_end()
            )));
        }
        let relative_start =
            usize::try_from(source_range.start - self.logical_start()).map_err(|_| {
                BinaryError::invalid_data("byte segment range start does not fit usize")
            })?;
        let relative_end = usize::try_from(source_range.end - self.logical_start())
            .map_err(|_| BinaryError::invalid_data("byte segment range end does not fit usize"))?;
        let backing_start = self
            .backing_range
            .start
            .checked_add(relative_start)
            .ok_or_else(|| BinaryError::invalid_data("byte segment backing start overflow"))?;
        let backing_end = self
            .backing_range
            .start
            .checked_add(relative_end)
            .ok_or_else(|| BinaryError::invalid_data("byte segment backing end overflow"))?;
        Self::from_backing_range(
            logical_start,
            self.backing.clone(),
            backing_start..backing_end,
        )
    }

    pub(super) fn logical_start(&self) -> u64 {
        self.logical_range.start
    }

    pub(super) fn logical_end(&self) -> u64 {
        self.logical_range.end
    }

    pub(super) fn contiguous_range(&self, logical_range: Range<u64>) -> Option<&[u8]> {
        if logical_range.start < self.logical_start() || logical_range.end > self.logical_end() {
            return None;
        }

        let relative_start = usize::try_from(logical_range.start - self.logical_start()).ok()?;
        let relative_end = usize::try_from(logical_range.end - self.logical_start()).ok()?;
        let backing_start = self.backing_range.start.checked_add(relative_start)?;
        let backing_end = self.backing_range.start.checked_add(relative_end)?;
        self.backing.as_slice().get(backing_start..backing_end)
    }
}

/// An immutable, logically contiguous byte image backed by any number of segments.
///
/// Construction and slicing retain the original shared allocations. They do not
/// concatenate the byte image.
#[doc(hidden)]
#[derive(Debug, PartialEq, Eq)]
pub struct SegmentedBytes {
    segments: Vec<ByteSegment>,
    len: u64,
}

impl SegmentedBytes {
    /// Validates and constructs a byte image from explicitly positioned segments.
    ///
    /// Empty segments are discarded. Every non-empty segment must begin exactly
    /// where the preceding segment ends, with the first segment beginning at zero.
    pub fn new(mut segments: Vec<ByteSegment>) -> Result<Self> {
        let mut expected_start = 0_u64;
        for segment in &segments {
            if segment.logical_start() != expected_start {
                return Err(BinaryError::invalid_data(format!(
                    "non-contiguous byte segment: expected logical offset {expected_start}, got {}",
                    segment.logical_start()
                )));
            }

            expected_start = segment.logical_end();
        }

        segments.retain(|segment| !segment.is_empty());
        if segments.is_empty() {
            segments = Vec::new();
        }

        Ok(Self {
            segments,
            len: expected_start,
        })
    }

    /// Constructs a one-segment byte image from an immutable allocation.
    pub fn from_contiguous(bytes: Arc<[u8]>) -> Result<Self> {
        let mut segments = Vec::new();
        try_push_segment(&mut segments, ByteSegment::new(0, bytes)?)?;
        Self::new(segments)
    }

    /// Constructs a byte image from sequential immutable allocations.
    pub fn from_arcs(segments: impl IntoIterator<Item = Arc<[u8]>>) -> Result<Self> {
        let mut logical_start = 0_u64;
        let mut positioned = Vec::new();
        for bytes in segments {
            let segment = ByteSegment::new(logical_start, bytes)?;
            logical_start = segment.logical_end();
            try_push_segment(&mut positioned, segment)?;
        }
        Self::new(positioned)
    }

    /// Returns an empty byte image.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            segments: Vec::new(),
            len: 0,
        }
    }

    /// Returns the logical byte length.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Returns whether the image contains no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the retained segment metadata.
    #[must_use]
    pub fn segments(&self) -> &[ByteSegment] {
        self.segments.as_slice()
    }

    /// Returns the retained metadata capacity without allocating.
    #[must_use]
    pub fn segment_capacity(&self) -> usize {
        self.segments.capacity()
    }

    /// Returns the complete image when it occupies one contiguous backing slice.
    #[must_use]
    pub fn contiguous(&self) -> Option<&[u8]> {
        self.contiguous_range(0..self.len)
    }

    /// Creates a rebased zero-copy view of a contained logical range.
    pub fn subrange(&self, range: Range<u64>) -> Result<Self> {
        validate_range("segmented byte subrange", &range, self.len)?;
        if range.is_empty() {
            return Ok(Self::empty());
        }

        let first_segment = self.segment_index(range.start).ok_or_else(|| {
            BinaryError::invalid_data(format!(
                "segmented byte subrange starts outside the image at {}",
                range.start
            ))
        })?;
        let end_segment = self
            .segments
            .partition_point(|segment| segment.logical_start() < range.end);
        let overlapping_segments = end_segment - first_segment;
        let mut view = Vec::new();
        view.try_reserve(overlapping_segments)
            .map_err(|error| allocation_error(overlapping_segments, error))?;

        for segment in &self.segments[first_segment..end_segment] {
            let overlap_start = segment.logical_start().max(range.start);
            let overlap_end = segment.logical_end().min(range.end);
            if overlap_start >= overlap_end {
                continue;
            }

            let relative_start = usize::try_from(overlap_start - segment.logical_start())
                .map_err(|_| BinaryError::invalid_data("segment offset does not fit in usize"))?;
            let relative_end = usize::try_from(overlap_end - segment.logical_start())
                .map_err(|_| BinaryError::invalid_data("segment offset does not fit in usize"))?;
            let backing_start = segment
                .backing_range
                .start
                .checked_add(relative_start)
                .ok_or_else(|| BinaryError::invalid_data("segment backing offset overflow"))?;
            let backing_end = segment
                .backing_range
                .start
                .checked_add(relative_end)
                .ok_or_else(|| BinaryError::invalid_data("segment backing offset overflow"))?;
            let rebased_start = overlap_start - range.start;
            view.push(ByteSegment::from_backing_range(
                rebased_start,
                segment.backing.clone(),
                backing_start..backing_end,
            )?);
        }

        let view = Self::new(view)?;
        let expected_len = range.end - range.start;
        if view.len != expected_len {
            return Err(BinaryError::invalid_data(format!(
                "segmented byte subrange produced {} bytes, expected {expected_len}",
                view.len
            )));
        }
        Ok(view)
    }

    pub(super) fn segment_index(&self, offset: u64) -> Option<usize> {
        if offset >= self.len {
            return None;
        }
        let index = self
            .segments
            .partition_point(|segment| segment.logical_end() <= offset);
        (index < self.segments.len()).then_some(index)
    }

    pub(super) fn contiguous_range(&self, range: Range<u64>) -> Option<&[u8]> {
        if validate_range("contiguous byte range", &range, self.len).is_err() {
            return None;
        }
        if range.is_empty() {
            return Some(&[]);
        }
        let index = self.segment_index(range.start)?;
        self.segments.get(index)?.contiguous_range(range)
    }
}

fn try_push_segment(segments: &mut Vec<ByteSegment>, segment: ByteSegment) -> Result<()> {
    segments
        .try_reserve(1)
        .map_err(|error| allocation_error(1, error))?;
    segments.push(segment);
    Ok(())
}

fn validate_backing_range(range: &Range<usize>, backing_len: usize) -> Result<()> {
    if range.start > range.end || range.end > backing_len {
        return Err(BinaryError::invalid_data(format!(
            "byte segment backing range {}..{} is outside allocation length {backing_len}",
            range.start, range.end
        )));
    }
    Ok(())
}
