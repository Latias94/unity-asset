use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::ops::{Deref, Range};
use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;
use unity_asset_binary::object::UnityObject;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ContractError, Diagnostic, ObjectAddress, RevisionedObjectHandle,
    SourceFingerprint, SourceId, SourceKind, SourceLocator, UnityClass, UnityDocument, WorkspaceId,
    WorkspaceRevision,
};
use unity_asset_yaml::YamlDocument;

use super::source_catalog::{CatalogAllocationUnit, CatalogError, SourceLocationKind};
use super::state::WorkspaceStateError;
use super::store::SourceStoreError;
use crate::{BinaryError, BinaryObjectIdentityError};

pub(crate) mod sealed {
    pub trait Sealed {}
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
    physical_origin: PathBuf,
}

impl WorkspaceSource {
    pub(crate) fn new(
        id: SourceId,
        locator: SourceLocator,
        fingerprint: SourceFingerprint,
        parent: Option<SourceId>,
        location: SourceLocationKind,
        physical_origin: PathBuf,
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
    pub fn physical_origin(&self) -> &std::path::Path {
        &self.physical_origin
    }
}

/// One copy-free byte range retained by a workspace snapshot.
#[derive(Debug, Clone)]
pub struct WorkspaceBytes {
    source: SourceId,
    backing: Arc<[u8]>,
    range: Range<usize>,
}

impl WorkspaceBytes {
    pub(crate) fn new(source: SourceId, backing: Arc<[u8]>, range: Range<usize>) -> Self {
        debug_assert!(range.start <= range.end && range.end <= backing.len());
        Self {
            source,
            backing,
            range,
        }
    }

    #[must_use]
    pub const fn source(&self) -> SourceId {
        self.source
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.backing[self.range.clone()]
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.range.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }
}

impl Deref for WorkspaceBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_bytes()
    }
}

/// Format-specific value behind one revision-bound object handle.
#[derive(Debug, Clone)]
pub enum WorkspaceObjectValue {
    Binary(Box<UnityObject>),
    Yaml(WorkspaceYamlObject),
}

/// Copy-free YAML object view retaining its immutable source document.
#[derive(Debug, Clone)]
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
    pub fn document_index(&self) -> usize {
        self.document_index
    }

    #[must_use]
    pub fn class(&self) -> &UnityClass {
        &self.document.entries()[self.document_index]
    }
}

/// Owned object inspection result tied to the handle used for the read.
#[derive(Debug, Clone)]
pub struct WorkspaceObject {
    handle: RevisionedObjectHandle,
    value: WorkspaceObjectValue,
}

impl WorkspaceObject {
    pub(crate) fn new(handle: RevisionedObjectHandle, value: WorkspaceObjectValue) -> Self {
        Self { handle, value }
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
        match &self.value {
            WorkspaceObjectValue::Binary(object) => object.as_unity_class(),
            WorkspaceObjectValue::Yaml(object) => object.class(),
        }
    }

    #[must_use]
    pub fn into_value(self) -> WorkspaceObjectValue {
        self.value
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

    fn read_object(
        &self,
        handle: &RevisionedObjectHandle,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceObject, WorkspaceError>;

    fn read_source_range(
        &self,
        source: SourceId,
        offset: u64,
        size: u64,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceBytes, WorkspaceError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WorkspaceSourceIdentityError {
    #[error("SerializedFile contains a zero path ID")]
    ZeroBinaryPathId,
    #[error("SerializedFile contains duplicate path IDs")]
    DuplicateBinaryPathId,
    #[error("YAML source contains duplicate object anchors")]
    DuplicateYamlAnchor,
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
    #[error("object identity is ambiguous in source {source_id:?}: {matches} matches")]
    AmbiguousObject { source_id: SourceId, matches: usize },
    #[error("source byte range overflows: offset={offset}, size={size}")]
    RangeOverflow { offset: u64, size: u64 },
    #[error("source byte range {offset}..{end} exceeds source {source_id:?} length {source_len}")]
    RangeOutOfBounds {
        source_id: SourceId,
        offset: u64,
        end: u64,
        source_len: usize,
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
