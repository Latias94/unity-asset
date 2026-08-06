use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;
use unity_asset_binary::asset::ObjectInfo;
use unity_asset_binary::object::UnityObject;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ContractError, Diagnostic, DigestV1, ObjectAddress,
    RevisionedObjectHandle, SourceFingerprint, SourceId, SourceKind, SourceLocator, UnityClass,
    UnityDocument, VerifiedSourceImage, WorkspaceId, WorkspaceRevision,
};
use unity_asset_write::artifact::{
    ArtifactBuildError, ArtifactHandle, ArtifactReader, PreparedArtifact, PreparedArtifactFormat,
    PreparedArtifactKind, PreparedArtifactSet,
};
use unity_asset_yaml::YamlDocument;

use super::source_catalog::{CatalogAllocationUnit, CatalogError, SourceLocationKind};
use super::state::SourceStoreError;
use super::state::WorkspaceStateError;
use crate::schema::SchemaProvenance;
use crate::{BinaryError, BinaryObjectIdentityError};

pub(crate) mod sealed {
    use unity_asset_core::{AssetLoadBudget, SourceId};

    use super::super::ReferenceViewParts;
    use super::{SourceObjectDescriptor, WorkspaceError, WorkspaceObject};

    pub(crate) trait Sealed {
        fn reference_view_parts(&self) -> ReferenceViewParts<'_>;

        fn object_count_in_source(
            &self,
            source: SourceId,
            budget: &mut AssetLoadBudget,
        ) -> Result<usize, WorkspaceError>;

        fn object_descriptor_at_in_source(
            &self,
            source: SourceId,
            index: usize,
            budget: &mut AssetLoadBudget,
        ) -> Result<SourceObjectDescriptor, WorkspaceError>;

        fn read_object_at_in_source(
            &self,
            descriptor: &SourceObjectDescriptor,
            budget: &mut AssetLoadBudget,
        ) -> Result<WorkspaceObject, WorkspaceError>;
    }
}

/// Metadata-only object-table projection used by private source-local algorithms.
#[derive(Debug, Clone)]
pub struct SourceObjectDescriptor {
    handle: RevisionedObjectHandle,
    class_id: i32,
    ordinal: usize,
}

impl SourceObjectDescriptor {
    pub(crate) const fn new(handle: RevisionedObjectHandle, class_id: i32, ordinal: usize) -> Self {
        Self {
            handle,
            class_id,
            ordinal,
        }
    }

    pub(crate) const fn handle(&self) -> &RevisionedObjectHandle {
        &self.handle
    }

    pub(crate) const fn class_id(&self) -> i32 {
        self.class_id
    }

    pub(crate) const fn ordinal(&self) -> usize {
        self.ordinal
    }

    pub(super) fn with_revision(mut self, revision: WorkspaceRevision) -> Self {
        self.handle = self.handle.with_revision(revision);
        self
    }
}

/// Immutable source metadata projected from one workspace revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSource {
    id: SourceId,
    kind: SourceKind,
    locator: SourceLocator,
    fingerprint: SourceFingerprint,
    parent: Option<SourceId>,
    location: SourceLocationKind,
    physical_origin: Option<PathBuf>,
}

impl WorkspaceSource {
    pub(crate) fn new(
        id: SourceId,
        locator: SourceLocator,
        fingerprint: SourceFingerprint,
        parent: Option<SourceId>,
        location: SourceLocationKind,
        physical_origin: Option<PathBuf>,
    ) -> Self {
        Self {
            id,
            kind: id.kind(),
            locator,
            fingerprint,
            parent,
            location,
            physical_origin,
        }
    }

    #[must_use]
    pub const fn id(&self) -> SourceId {
        self.id
    }

    #[must_use]
    pub const fn kind(&self) -> SourceKind {
        self.kind
    }

    #[must_use]
    pub const fn locator(&self) -> &SourceLocator {
        &self.locator
    }

    #[must_use]
    pub const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }

    #[must_use]
    pub const fn parent(&self) -> Option<SourceId> {
        self.parent
    }

    #[must_use]
    pub const fn location(&self) -> SourceLocationKind {
        self.location
    }

    #[must_use]
    pub fn physical_origin(&self) -> Option<&std::path::Path> {
        self.physical_origin.as_deref()
    }
}

/// One immutable, reader-backed byte range retained by a workspace view.
///
/// A range may span non-contiguous storage. Use [`Self::contiguous`] only as an
/// optional fast path; [`Self::reader`] and [`Self::copy_to`] work for every
/// backing without materializing the complete range.
#[derive(Debug, Clone)]
pub struct WorkspaceByteRange {
    source: SourceId,
    fingerprint: SourceFingerprint,
    len: u64,
    backing: WorkspaceByteRangeBacking,
}

impl WorkspaceByteRange {
    pub(super) fn from_committed_source(
        source: SourceId,
        image: VerifiedSourceImage,
        range: Range<usize>,
    ) -> Result<Self, WorkspaceError> {
        debug_assert_eq!(source.kind(), image.kind());
        let source_len =
            u64::try_from(image.as_bytes().len()).map_err(|_| BudgetError::ArithmeticOverflow {
                resource: "workspace_source_length",
            })?;
        let offset = u64::try_from(range.start).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "workspace_source_range_start",
        })?;
        let end = u64::try_from(range.end).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "workspace_source_range_end",
        })?;
        if range.start > range.end || range.end > image.as_bytes().len() {
            return Err(WorkspaceError::RangeOutOfBounds {
                source_id: source,
                offset,
                end,
                source_len,
            });
        }
        let len = end - offset;
        Ok(Self {
            source,
            fingerprint: image.fingerprint(),
            len,
            backing: WorkspaceByteRangeBacking::Committed { image, range },
        })
    }

    pub(super) fn from_prepared(
        source: SourceId,
        fingerprint: SourceFingerprint,
        artifacts: Arc<PreparedArtifactSet>,
        handle: ArtifactHandle,
        range: Range<u64>,
    ) -> Result<Self, WorkspaceError> {
        if source.kind() != fingerprint.kind() {
            return Err(WorkspaceError::PreparedSourceKindMismatch {
                source_id: source,
                fingerprint_kind: fingerprint.kind(),
            });
        }

        let artifact = artifacts
            .artifact(handle)
            .map_err(|error| WorkspaceError::PreparedArtifact(Box::new(error)))?;
        validate_prepared_artifact(source, fingerprint, artifact)?;
        let artifact_len = artifact.len();
        if range.start > range.end || range.end > artifact_len {
            return Err(WorkspaceError::RangeOutOfBounds {
                source_id: source,
                offset: range.start,
                end: range.end,
                source_len: artifact_len,
            });
        }
        let len = range.end - range.start;
        Ok(Self {
            source,
            fingerprint,
            len,
            backing: WorkspaceByteRangeBacking::Prepared {
                artifacts,
                handle,
                range,
                artifact_len,
            },
        })
    }

    /// Returns the source that owns this logical range.
    #[must_use]
    pub const fn source(&self) -> SourceId {
        self.source
    }

    /// Returns the fingerprint of the complete owning source, not only this range.
    #[must_use]
    pub const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }

    /// Returns this range as one borrowed slice when its backing is contiguous.
    ///
    /// Callers must not require this optimization: prepared workspace views may
    /// retain the same logical range as multiple immutable artifact segments.
    #[must_use]
    pub fn contiguous(&self) -> Option<&[u8]> {
        match &self.backing {
            WorkspaceByteRangeBacking::Committed { image, range } => {
                Some(&image.as_bytes()[range.clone()])
            }
            WorkspaceByteRangeBacking::Prepared {
                artifacts,
                handle,
                range,
                artifact_len,
            } => artifacts
                .artifact(*handle)
                .ok()
                .filter(|artifact| artifact.len() == *artifact_len)?
                .contiguous_range(range.clone()),
        }
    }

    /// Returns the logical range length in bytes.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Returns whether this logical range contains no bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Creates an independent reader bounded to this range at position zero.
    ///
    /// All seek offsets are relative to the range rather than the owning source.
    /// Seeking before position zero or beyond [`Self::len`] returns
    /// [`io::ErrorKind::InvalidInput`].
    #[must_use]
    pub fn reader(&self) -> WorkspaceByteRangeReader<'_> {
        let backing = match &self.backing {
            WorkspaceByteRangeBacking::Committed { image, range } => {
                WorkspaceByteRangeReaderBacking::Committed(Cursor::new(
                    &image.as_bytes()[range.clone()],
                ))
            }
            WorkspaceByteRangeBacking::Prepared {
                artifacts,
                handle,
                range,
                artifact_len,
            } => match artifacts.artifact(*handle) {
                Ok(artifact) if artifact.len() == *artifact_len => {
                    match PreparedRangeReader::new(artifact, range.clone()) {
                        Ok(reader) => WorkspaceByteRangeReaderBacking::Prepared(reader),
                        Err(error) => WorkspaceByteRangeReaderBacking::Invalid(Some(error)),
                    }
                }
                Ok(_) => WorkspaceByteRangeReaderBacking::Invalid(Some(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "prepared artifact length changed after range validation",
                ))),
                Err(error) => WorkspaceByteRangeReaderBacking::Invalid(Some(io::Error::new(
                    io::ErrorKind::InvalidData,
                    error,
                ))),
            },
        };
        WorkspaceByteRangeReader { backing }
    }

    /// Streams this range to `output` without materializing it as one allocation.
    pub fn copy_to<W>(&self, output: &mut W) -> io::Result<u64>
    where
        W: Write + ?Sized,
    {
        let copied = io::copy(&mut self.reader(), output)?;
        if copied != self.len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "workspace byte range ended before its declared length",
            ));
        }
        Ok(copied)
    }
}

/// An independent cursor over one [`WorkspaceByteRange`].
pub struct WorkspaceByteRangeReader<'range> {
    backing: WorkspaceByteRangeReaderBacking<'range>,
}

/// Private variants keep each backing owner and its validated range inseparable.
#[derive(Debug, Clone)]
enum WorkspaceByteRangeBacking {
    Committed {
        image: VerifiedSourceImage,
        range: Range<usize>,
    },
    Prepared {
        artifacts: Arc<PreparedArtifactSet>,
        handle: ArtifactHandle,
        range: Range<u64>,
        artifact_len: u64,
    },
}

enum WorkspaceByteRangeReaderBacking<'range> {
    Committed(Cursor<&'range [u8]>),
    Prepared(PreparedRangeReader<'range>),
    Invalid(Option<io::Error>),
}

struct PreparedRangeReader<'range> {
    reader: ArtifactReader<'range>,
    start: u64,
    len: u64,
    position: u64,
}

impl<'range> PreparedRangeReader<'range> {
    fn new(artifact: &'range PreparedArtifact, range: Range<u64>) -> io::Result<Self> {
        let len = range.end.checked_sub(range.start).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared artifact range is inverted",
            )
        })?;
        if range.end > artifact.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared artifact range exceeds the artifact length",
            ));
        }
        let mut reader = artifact.reader();
        let actual = reader.seek(SeekFrom::Start(range.start))?;
        if actual != range.start {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared artifact reader sought to an unexpected position",
            ));
        }
        Ok(Self {
            reader,
            start: range.start,
            len,
            position: 0,
        })
    }

    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.position == self.len {
            return Ok(0);
        }
        let remaining = self.len - self.position;
        let output_len = u64::try_from(output.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "prepared range read buffer length does not fit u64",
            )
        })?;
        let requested = usize::try_from(remaining.min(output_len)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "prepared range read length does not fit usize",
            )
        })?;
        let read = self.reader.read(&mut output[..requested])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "prepared artifact ended before the validated range",
            ));
        }
        let read_u64 = u64::try_from(read).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared range read count does not fit u64",
            )
        })?;
        self.position = self.position.checked_add(read_u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared range reader position overflow",
            )
        })?;
        if self.position > self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared artifact reader exceeded the validated range",
            ));
        }
        Ok(read)
    }

    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = bounded_seek_target(self.position, self.len, position)?;
        let absolute = self.start.checked_add(target).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "prepared range seek target overflow",
            )
        })?;
        let actual = self.reader.seek(SeekFrom::Start(absolute))?;
        if actual != absolute {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared artifact reader sought to an unexpected position",
            ));
        }
        self.position = target;
        Ok(target)
    }
}

impl Read for WorkspaceByteRangeReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        match &mut self.backing {
            WorkspaceByteRangeReaderBacking::Committed(reader) => reader.read(output),
            WorkspaceByteRangeReaderBacking::Prepared(reader) => reader.read(output),
            WorkspaceByteRangeReaderBacking::Invalid(error) => Err(take_reader_error(error)),
        }
    }
}

impl Seek for WorkspaceByteRangeReader<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match &mut self.backing {
            WorkspaceByteRangeReaderBacking::Committed(reader) => {
                let len = u64::try_from(reader.get_ref().len()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "workspace byte range length does not fit u64",
                    )
                })?;
                let target = bounded_seek_target(reader.position(), len, position)?;
                reader.set_position(target);
                Ok(target)
            }
            WorkspaceByteRangeReaderBacking::Prepared(reader) => reader.seek(position),
            WorkspaceByteRangeReaderBacking::Invalid(error) => Err(take_reader_error(error)),
        }
    }
}

fn take_reader_error(error: &mut Option<io::Error>) -> io::Error {
    match error.take() {
        Some(error) => error,
        None => io::Error::new(
            io::ErrorKind::InvalidData,
            "workspace byte range backing remains invalid",
        ),
    }
}

fn bounded_seek_target(current: u64, len: u64, position: SeekFrom) -> io::Result<u64> {
    let target = match position {
        SeekFrom::Start(offset) => i128::from(offset),
        SeekFrom::End(delta) => i128::from(len) + i128::from(delta),
        SeekFrom::Current(delta) => i128::from(current) + i128::from(delta),
    };
    if !(0..=i128::from(len)).contains(&target) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace byte range seek is outside the range",
        ));
    }
    u64::try_from(target).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace byte range seek target does not fit u64",
        )
    })
}

pub(super) fn validate_prepared_artifact(
    source: SourceId,
    fingerprint: SourceFingerprint,
    artifact: &PreparedArtifact,
) -> Result<(), WorkspaceError> {
    let format = artifact.format();
    if !prepared_format_matches_source(format, source.kind()) {
        return Err(WorkspaceError::PreparedArtifactKindMismatch {
            source_id: source,
            source_kind: source.kind(),
            artifact_kind: format.kind(),
        });
    }
    if let PreparedArtifactFormat::VerbatimSource(proof) = format {
        if proof.source_id() != source {
            return Err(WorkspaceError::PreparedArtifactSourceProvenanceMismatch {
                expected: Box::new(source),
                actual: proof.source_id(),
            });
        }
        if proof.fingerprint() != fingerprint {
            return Err(
                WorkspaceError::PreparedArtifactFingerprintProvenanceMismatch {
                    expected: fingerprint,
                    actual: proof.fingerprint(),
                },
            );
        }
    }
    if artifact.digest() != fingerprint.digest() {
        return Err(WorkspaceError::PreparedArtifactDigestMismatch {
            source_id: Box::new(source),
            expected: fingerprint.digest(),
            actual: artifact.digest(),
        });
    }
    let inspected_len =
        inspected_artifact_len(format).ok_or(WorkspaceError::PreparedArtifactKindMismatch {
            source_id: source,
            source_kind: source.kind(),
            artifact_kind: format.kind(),
        })?;
    if inspected_len != artifact.len() {
        return Err(WorkspaceError::PreparedArtifactLengthMismatch {
            source_id: source,
            inspected: inspected_len,
            actual: artifact.len(),
        });
    }
    Ok(())
}

fn prepared_format_matches_source(format: &PreparedArtifactFormat, kind: SourceKind) -> bool {
    match format {
        PreparedArtifactFormat::SerializedFile(_) => kind == SourceKind::SerializedFile,
        PreparedArtifactFormat::AssetBundle(_) => kind == SourceKind::AssetBundle,
        PreparedArtifactFormat::WebFile(_) => kind == SourceKind::WebFile,
        PreparedArtifactFormat::StreamedResource(_) => kind == SourceKind::StreamedResource,
        PreparedArtifactFormat::Yaml(_) => kind == SourceKind::Yaml,
        PreparedArtifactFormat::VerbatimSource(_) => true,
        _ => false,
    }
}

fn inspected_artifact_len(format: &PreparedArtifactFormat) -> Option<u64> {
    match format {
        PreparedArtifactFormat::SerializedFile(proof) => Some(proof.declared_file_size()),
        PreparedArtifactFormat::AssetBundle(proof) => Some(proof.stats().encoded_bytes()),
        PreparedArtifactFormat::WebFile(proof) => Some(proof.stats().encoded_bytes()),
        PreparedArtifactFormat::StreamedResource(proof) => Some(proof.length()),
        PreparedArtifactFormat::Yaml(proof) => Some(proof.encoded_bytes()),
        PreparedArtifactFormat::VerbatimSource(proof) => Some(proof.length()),
        _ => None,
    }
}

/// Format-specific value behind one revision-bound object handle.
#[derive(Debug, Clone)]
pub enum WorkspaceObjectValue {
    Binary(Arc<UnityObject>),
    Yaml(WorkspaceYamlObject),
}

impl WorkspaceObjectValue {
    #[must_use]
    pub fn class(&self) -> &UnityClass {
        match self {
            Self::Binary(object) => object.as_unity_class(),
            Self::Yaml(object) => object.class(),
        }
    }
}

/// Copy-free YAML object view retaining the complete document that produced its class.
#[derive(Clone)]
pub struct WorkspaceYamlObject {
    document: Arc<YamlDocument>,
    document_index: usize,
}

impl WorkspaceYamlObject {
    pub(crate) fn new(document: Arc<YamlDocument>, document_index: usize) -> Self {
        debug_assert!(document_index < document.entries().len());
        Self {
            document,
            document_index,
        }
    }

    #[must_use]
    pub const fn document_index(&self) -> usize {
        self.document_index
    }

    #[must_use]
    pub fn class(&self) -> &UnityClass {
        &self.document.entries()[self.document_index]
    }
}

impl fmt::Debug for WorkspaceYamlObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let class = self.class();
        formatter
            .debug_struct("WorkspaceYamlObject")
            .field("document_index", &self.document_index())
            .field("class_id", &class.class_id())
            .field("class_name", &class.class_name())
            .field("anchor", &class.anchor())
            .finish()
    }
}

/// Owned object inspection result tied to the handle used for the read.
#[derive(Debug, Clone)]
pub struct WorkspaceObject {
    handle: RevisionedObjectHandle,
    value: WorkspaceObjectValue,
    schema: Arc<SchemaProvenance>,
}

impl WorkspaceObject {
    pub(crate) fn new(
        handle: RevisionedObjectHandle,
        value: WorkspaceObjectValue,
        schema: SchemaProvenance,
    ) -> Self {
        Self {
            handle,
            value,
            schema: Arc::new(schema),
        }
    }

    pub(super) fn from_shared(
        handle: RevisionedObjectHandle,
        value: WorkspaceObjectValue,
        schema: Arc<SchemaProvenance>,
    ) -> Self {
        Self {
            handle,
            value,
            schema,
        }
    }

    #[must_use]
    pub const fn handle(&self) -> &RevisionedObjectHandle {
        &self.handle
    }

    #[must_use]
    pub const fn value(&self) -> &WorkspaceObjectValue {
        &self.value
    }

    #[must_use]
    pub fn class(&self) -> &UnityClass {
        self.value.class()
    }

    /// Returns the trusted schema identity used to materialize this object.
    #[must_use]
    pub fn schema_provenance(&self) -> &SchemaProvenance {
        &self.schema
    }

    #[must_use]
    pub fn into_value(self) -> WorkspaceObjectValue {
        self.value
    }

    pub(super) fn into_shared_parts(
        self,
    ) -> (
        RevisionedObjectHandle,
        WorkspaceObjectValue,
        Arc<SchemaProvenance>,
    ) {
        (self.handle, self.value, self.schema)
    }

    pub(super) fn with_revision(mut self, revision: WorkspaceRevision) -> Self {
        self.handle = self.handle.with_revision(revision);
        self
    }

    pub(super) fn with_exact_binary_info(
        mut self,
        info: ObjectInfo,
    ) -> Result<Self, WorkspaceError> {
        let WorkspaceObjectValue::Binary(object) = &mut self.value else {
            return Err(WorkspaceError::operation(
                "prepared binary object projection",
                io::Error::other("binary metadata was applied to a YAML object"),
            ));
        };
        let object = Arc::get_mut(object).ok_or_else(|| {
            WorkspaceError::operation(
                "prepared binary object projection",
                io::Error::other("fresh binary object unexpectedly shared its owned projection"),
            )
        })?;
        if object.path_id() != info.path_id() || object.class_id() != info.class_id() {
            return Err(WorkspaceError::operation(
                "prepared binary object projection",
                io::Error::other("exact binary metadata changed object identity or class"),
            ));
        }
        object.info = info;
        Ok(self)
    }
}

/// Structured result for lookups that must not trigger hidden source loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceLookup<T> {
    Resolved(T),
    Unloaded,
    Missing,
    Ambiguous { candidates: Vec<T> },
    Invalid { diagnostic: Diagnostic },
}

/// Common immutable query interface for committed snapshots and future prepared views.
#[allow(private_bounds)]
pub trait WorkspaceView: sealed::Sealed + Send + Sync {
    fn workspace_id(&self) -> WorkspaceId;

    fn revision(&self) -> WorkspaceRevision;

    fn sources(&self, budget: &mut AssetLoadBudget)
    -> Result<Vec<WorkspaceSource>, WorkspaceError>;

    fn source(
        &self,
        source: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceLookup<WorkspaceSource>, WorkspaceError>;

    fn resolve_source(
        &self,
        locator: &SourceLocator,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceLookup<WorkspaceSource>, WorkspaceError>;

    fn objects(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<RevisionedObjectHandle>, WorkspaceError>;

    fn resolve_object(
        &self,
        address: &ObjectAddress,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceLookup<RevisionedObjectHandle>, WorkspaceError>;

    /// Projects a revision-bound handle into its versioned persistent address.
    fn object_address(
        &self,
        handle: &RevisionedObjectHandle,
        budget: &mut AssetLoadBudget,
    ) -> Result<ObjectAddress, WorkspaceError>;

    fn read_object(
        &self,
        handle: &RevisionedObjectHandle,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceObject, WorkspaceError>;

    /// Returns the exact byte length retained for one source in this view.
    fn source_length(&self, source: SourceId) -> Result<u64, WorkspaceError>;

    fn read_source_range(
        &self,
        source: SourceId,
        offset: u64,
        size: u64,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceByteRange, WorkspaceError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WorkspaceSourceIdentityError {
    #[error("SerializedFile contains a zero path ID")]
    ZeroBinaryPathId,
    #[error("SerializedFile contains duplicate path IDs")]
    DuplicateBinaryPathId,
    #[error("YAML source contains duplicate object file IDs")]
    DuplicateYamlFileId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAllocationUnit {
    Bytes,
    Elements,
    Entries,
    Slots,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceSourceContainer {
    Archive,
    AssetBundle,
    WebFile,
}

impl fmt::Display for WorkspaceSourceContainer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Archive => "ZIP archive",
            Self::AssetBundle => "AssetBundle",
            Self::WebFile => "WebFile",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WorkspaceSourceMemberIdentityError {
    #[error("the member name is empty")]
    Empty,
    #[error("the member name exceeds the portable path limit")]
    TooLong,
    #[error("the member name is not stable UTF-8")]
    UnstableEncoding,
    #[error("the member name is absolute")]
    Absolute,
    #[error("the member name contains a backslash")]
    Backslash,
    #[error("the member name contains a NUL or control character")]
    ControlCharacter,
    #[error("the member name contains an unsafe path component")]
    TraversalComponent,
    #[error(transparent)]
    Contract(#[from] ContractError),
}

impl fmt::Display for WorkspaceAllocationUnit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Bytes => "bytes",
            Self::Elements => "elements",
            Self::Entries => "entries",
            Self::Slots => "slots",
        })
    }
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error("failed to access workspace source {path:?}: {message}")]
    Io {
        path: PathBuf,
        kind: io::ErrorKind,
        message: String,
    },
    #[error("invalid workspace source {path:?}: {message}")]
    InvalidSource { path: PathBuf, message: String },
    #[error("unsupported workspace source {path:?}")]
    UnsupportedSource { path: PathBuf },
    #[error("invalid {source_kind:?} source identity: {reason}")]
    InvalidSourceIdentity {
        source_kind: SourceKind,
        reason: WorkspaceSourceIdentityError,
    },
    #[error("workspace source {path:?} changed while it was being read")]
    SourceChanged { path: PathBuf },
    #[error("workspace source {path:?} is too large for this platform: {length} bytes")]
    SourceTooLarge { path: PathBuf, length: u64 },
    #[error("failed to allocate {requested} {unit} for {resource}: {message}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        unit: WorkspaceAllocationUnit,
        message: String,
    },
    #[error("source is not a loaded workspace root: {0:?}")]
    NotRootSource(SourceId),
    #[error("source is not loaded in this workspace revision: {0:?}")]
    MissingSource(SourceId),
    #[error("object is not present in this workspace revision: {0:?}")]
    MissingObject(Box<ObjectAddress>),
    #[error(
        "source {source_id:?} changed after inspection: expected {expected}, observed {actual}"
    )]
    ObservedSourceChanged {
        source_id: Box<SourceId>,
        expected: SourceFingerprint,
        actual: SourceFingerprint,
    },
    #[error("object identity is ambiguous in source {source_id:?}: {matches} matches")]
    AmbiguousObject { source_id: SourceId, matches: usize },
    #[error("source byte range overflows: offset={offset}, size={size}")]
    RangeOverflow { offset: u64, size: u64 },
    #[error("source byte range {offset}..{end} exceeds source {source_id:?} length {source_len}")]
    RangeOutOfBounds {
        source_id: SourceId,
        offset: u64,
        end: u64,
        source_len: u64,
    },
    #[error("prepared artifact capability validation failed: {0}")]
    PreparedArtifact(#[source] Box<ArtifactBuildError>),
    #[error(
        "prepared source {source_id:?} kind does not match fingerprint kind {fingerprint_kind:?}"
    )]
    PreparedSourceKindMismatch {
        source_id: SourceId,
        fingerprint_kind: SourceKind,
    },
    #[error(
        "prepared artifact kind {artifact_kind:?} cannot represent source {source_id:?} kind {source_kind:?}"
    )]
    PreparedArtifactKindMismatch {
        source_id: SourceId,
        source_kind: SourceKind,
        artifact_kind: PreparedArtifactKind,
    },
    #[error("prepared artifact for source {source_id:?} has digest {actual}, expected {expected}")]
    PreparedArtifactDigestMismatch {
        source_id: Box<SourceId>,
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error(
        "prepared artifact for source {source_id:?} inspected {inspected} bytes but retains {actual}"
    )]
    PreparedArtifactLengthMismatch {
        source_id: SourceId,
        inspected: u64,
        actual: u64,
    },
    #[error("prepared verbatim artifact source is {actual:?}, expected {expected:?}")]
    PreparedArtifactSourceProvenanceMismatch {
        expected: Box<SourceId>,
        actual: SourceId,
    },
    #[error("prepared verbatim artifact fingerprint is {actual}, expected {expected}")]
    PreparedArtifactFingerprintProvenanceMismatch {
        expected: SourceFingerprint,
        actual: SourceFingerprint,
    },
    #[error("binary source parsing failed: {0}")]
    Binary(#[source] BinaryError),
    #[error("binary {container} member at wire ordinal {wire_ordinal} failed: {source}")]
    BinaryMember {
        container: WorkspaceSourceContainer,
        wire_ordinal: u64,
        #[source]
        source: BinaryError,
    },
    #[error("invalid {container} member identity at wire ordinal {wire_ordinal}: {reason}")]
    InvalidSourceMemberIdentity {
        container: WorkspaceSourceContainer,
        wire_ordinal: u64,
        #[source]
        reason: WorkspaceSourceMemberIdentityError,
    },
    #[error("workspace {operation} failed")]
    Operation {
        operation: &'static str,
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
}

impl WorkspaceError {
    pub(crate) fn io(path: impl Into<PathBuf>, error: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    pub(crate) fn operation(
        operation: &'static str,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::Operation {
            operation,
            source: Box::new(source),
        }
    }
}

impl From<CatalogError> for WorkspaceError {
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::InvalidIdentity(error) => Self::Contract(error),
            CatalogError::Budget(error) => Self::Budget(error),
            CatalogError::AllocationFailed {
                resource,
                requested,
                unit,
                message,
            } => Self::Allocation {
                resource,
                requested,
                unit: match unit {
                    CatalogAllocationUnit::Bytes => WorkspaceAllocationUnit::Bytes,
                    CatalogAllocationUnit::Elements => WorkspaceAllocationUnit::Elements,
                    CatalogAllocationUnit::Slots => WorkspaceAllocationUnit::Slots,
                },
                message,
            },
            CatalogError::AllocationSizeOverflow { resource } => {
                Self::Budget(BudgetError::ArithmeticOverflow { resource })
            }
            error => Self::operation("source catalog", error),
        }
    }
}

impl From<SourceStoreError> for WorkspaceError {
    fn from(error: SourceStoreError) -> Self {
        match error {
            SourceStoreError::Budget(error) => Self::Budget(error),
            SourceStoreError::AllocationFailed {
                resource,
                requested,
            } => Self::Allocation {
                resource,
                requested,
                unit: WorkspaceAllocationUnit::Entries,
                message: "allocator rejected the requested capacity".to_owned(),
            },
            SourceStoreError::RetainedSizeOverflow => {
                Self::Budget(BudgetError::ArithmeticOverflow {
                    resource: "source_store",
                })
            }
            error => Self::operation("source store", error),
        }
    }
}

impl From<WorkspaceStateError> for WorkspaceError {
    fn from(error: WorkspaceStateError) -> Self {
        match error {
            WorkspaceStateError::Budget(error) => Self::Budget(error),
            WorkspaceStateError::Catalog(error) => Self::from(*error),
            WorkspaceStateError::Store(error) => Self::from(*error),
            error => Self::operation("workspace state validation", error),
        }
    }
}

impl From<BinaryError> for WorkspaceError {
    fn from(error: BinaryError) -> Self {
        match error {
            BinaryError::Budget(error) => Self::Budget(error),
            BinaryError::ObjectIdentity(BinaryObjectIdentityError::ZeroPathId) => {
                Self::InvalidSourceIdentity {
                    source_kind: SourceKind::SerializedFile,
                    reason: WorkspaceSourceIdentityError::ZeroBinaryPathId,
                }
            }
            BinaryError::ObjectIdentity(BinaryObjectIdentityError::DuplicatePathId { .. }) => {
                Self::InvalidSourceIdentity {
                    source_kind: SourceKind::SerializedFile,
                    reason: WorkspaceSourceIdentityError::DuplicateBinaryPathId,
                }
            }
            error => Self::Binary(error),
        }
    }
}

#[cfg(test)]
mod tests;
