use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;
use unity_asset_core::{
    BundleMemberId, ContainmentKind, ContainmentStep, DigestBuildError, DigestV1, DigestV1Builder,
    ObjectAddress, ObjectId, ObjectKind, RevisionedObjectHandle, SourceAlias, SourceFingerprint,
    SourceId, SourceKind, SourceLocator, SourceMemberId, WorkspaceId, WorkspaceRevision,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Runtime filesystem binding. It is never serialized into a logical object address.
pub struct PhysicalOrigin(PathBuf);

impl PhysicalOrigin {
    pub fn from_existing_path(path: impl AsRef<Path>) -> Result<Self, PhysicalOriginError> {
        let requested = path.as_ref();
        if !requested.is_absolute() {
            return Err(PhysicalOriginError::NotAbsolute(requested.to_path_buf()));
        }
        #[cfg(windows)]
        validate_windows_origin_path(requested)?;

        let canonical = fs::canonicalize(requested)
            .map_err(|error| PhysicalOriginError::io(requested, error))?;
        #[cfg(windows)]
        validate_windows_origin_path(&canonical)?;
        let metadata =
            fs::metadata(&canonical).map_err(|error| PhysicalOriginError::io(&canonical, error))?;
        if !metadata.is_file() {
            return Err(PhysicalOriginError::NotRegularFile(canonical));
        }
        Ok(Self(canonical))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PhysicalOriginError {
    #[error("physical origin must be an absolute path: {0:?}")]
    NotAbsolute(PathBuf),
    #[error("failed to resolve physical origin {path:?}: {message}")]
    Io {
        path: PathBuf,
        kind: io::ErrorKind,
        message: String,
    },
    #[error("physical origin is not a regular file: {0:?}")]
    NotRegularFile(PathBuf),
    #[cfg(windows)]
    #[error("unsupported Windows path namespace for physical origin: {0:?}")]
    UnsupportedWindowsNamespace(PathBuf),
    #[cfg(windows)]
    #[error("Windows alternate data streams cannot be physical origins: {0:?}")]
    AlternateDataStream(PathBuf),
}

impl PhysicalOriginError {
    fn io(path: &Path, error: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

#[cfg(windows)]
fn validate_windows_origin_path(path: &Path) -> Result<(), PhysicalOriginError> {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(PhysicalOriginError::NotAbsolute(path.to_path_buf()));
    };
    match prefix.kind() {
        Prefix::Disk(_)
        | Prefix::UNC(_, _)
        | Prefix::VerbatimDisk(_)
        | Prefix::VerbatimUNC(_, _) => {}
        Prefix::DeviceNS(_) | Prefix::Verbatim(_) => {
            return Err(PhysicalOriginError::UnsupportedWindowsNamespace(
                path.to_path_buf(),
            ));
        }
    }

    if components.any(|component| {
        matches!(component, Component::Normal(value) if value.encode_wide().any(|unit| unit == u16::from(b':')))
    }) {
        return Err(PhysicalOriginError::AlternateDataStream(
            path.to_path_buf(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Placement shape exposed for structured source inspection.
pub enum SourceLocationKind {
    Root,
    ArchiveMember,
    WebFileMember,
    BundleMember,
    Sidecar,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Validated source declaration consumed by `SourceCatalog::register`.
pub struct SourceDescriptor {
    kind: SourceKind,
    placement: SourcePlacement,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SourcePlacement {
    Root {
        alias: SourceAlias,
        physical_origin: PhysicalOrigin,
    },
    Member {
        parent: SourceId,
        step: ContainmentStep,
        location_kind: SourceLocationKind,
    },
}

impl SourceDescriptor {
    #[must_use]
    pub fn root(kind: SourceKind, alias: SourceAlias, physical_origin: PhysicalOrigin) -> Self {
        Self {
            kind,
            placement: SourcePlacement::Root {
                alias,
                physical_origin,
            },
        }
    }

    pub fn archive_member(
        parent: SourceId,
        kind: SourceKind,
        member: SourceMemberId,
    ) -> Result<Self, CatalogError> {
        ensure_regular_member_kind(kind)?;
        Self::member(
            parent,
            SourceKind::Archive,
            kind,
            ContainmentKind::Archive,
            member,
            SourceLocationKind::ArchiveMember,
        )
    }

    pub fn webfile_member(
        parent: SourceId,
        kind: SourceKind,
        member: SourceMemberId,
    ) -> Result<Self, CatalogError> {
        ensure_regular_member_kind(kind)?;
        Self::member(
            parent,
            SourceKind::WebFile,
            kind,
            ContainmentKind::WebFile,
            member,
            SourceLocationKind::WebFileMember,
        )
    }

    pub fn bundle_member(
        parent: SourceId,
        kind: SourceKind,
        member: BundleMemberId,
    ) -> Result<Self, CatalogError> {
        ensure_regular_member_kind(kind)?;
        Self::member(
            parent,
            SourceKind::AssetBundle,
            kind,
            ContainmentKind::Bundle,
            member,
            SourceLocationKind::BundleMember,
        )
    }

    pub fn sidecar(parent: SourceId, member: SourceMemberId) -> Result<Self, CatalogError> {
        let containment = match parent.kind() {
            SourceKind::Archive => ContainmentKind::Archive,
            SourceKind::WebFile => ContainmentKind::WebFile,
            SourceKind::AssetBundle => ContainmentKind::Bundle,
            actual => {
                return Err(CatalogError::InvalidSidecarParentKind { parent, actual });
            }
        };
        Self::member(
            parent,
            parent.kind(),
            SourceKind::StreamedResource,
            containment,
            member,
            SourceLocationKind::Sidecar,
        )
    }

    fn member(
        parent: SourceId,
        expected_parent_kind: SourceKind,
        kind: SourceKind,
        containment: ContainmentKind,
        member: SourceMemberId,
        location_kind: SourceLocationKind,
    ) -> Result<Self, CatalogError> {
        if parent.kind() != expected_parent_kind {
            return Err(CatalogError::InvalidParentKind {
                parent,
                expected: expected_parent_kind,
                actual: parent.kind(),
            });
        }
        Ok(Self {
            kind,
            placement: SourcePlacement::Member {
                parent,
                step: ContainmentStep::new(containment, member),
                location_kind,
            },
        })
    }

    #[must_use]
    pub const fn kind(&self) -> SourceKind {
        self.kind
    }

    #[must_use]
    pub fn parent(&self) -> Option<SourceId> {
        match &self.placement {
            SourcePlacement::Root { .. } => None,
            SourcePlacement::Member { parent, .. } => Some(*parent),
        }
    }

    #[must_use]
    pub fn location_kind(&self) -> SourceLocationKind {
        match &self.placement {
            SourcePlacement::Root { .. } => SourceLocationKind::Root,
            SourcePlacement::Member { location_kind, .. } => *location_kind,
        }
    }

    #[must_use]
    pub fn root_alias(&self) -> Option<&SourceAlias> {
        match &self.placement {
            SourcePlacement::Root { alias, .. } => Some(alias),
            SourcePlacement::Member { .. } => None,
        }
    }

    #[must_use]
    pub fn member_id(&self) -> Option<&SourceMemberId> {
        match &self.placement {
            SourcePlacement::Root { .. } => None,
            SourcePlacement::Member { step, .. } => Some(step.member()),
        }
    }
}

#[derive(Debug, Clone)]
struct SourceRecord {
    descriptor: SourceDescriptor,
    fingerprint: SourceFingerprint,
    source_locator: SourceLocator,
    physical_origin: PhysicalOrigin,
    canonical_key: Vec<u8>,
}

/// Workspace-local authority for source ownership, physical bindings, and opaque identities.
#[derive(Debug, Clone)]
pub struct SourceCatalog {
    workspace: WorkspaceId,
    by_key: HashMap<Vec<u8>, SourceId>,
    by_id: BTreeMap<SourceId, SourceRecord>,
    by_locator: HashMap<SourceLocator, SourceId>,
    physical_roots: HashMap<PhysicalOrigin, SourceId>,
}

impl SourceCatalog {
    #[must_use]
    pub fn new(workspace: WorkspaceId) -> Self {
        Self {
            workspace,
            by_key: HashMap::new(),
            by_id: BTreeMap::new(),
            by_locator: HashMap::new(),
            physical_roots: HashMap::new(),
        }
    }

    pub fn register(
        &mut self,
        descriptor: SourceDescriptor,
        fingerprint: SourceFingerprint,
    ) -> Result<SourceId, CatalogError> {
        if descriptor.kind != fingerprint.kind() {
            return Err(CatalogError::SourceKindMismatch {
                expected: descriptor.kind,
                actual: fingerprint.kind(),
            });
        }

        let (source_locator, physical_origin) = self.resolve_placement(&descriptor)?;
        let key = canonical_source_key(descriptor.kind, &source_locator);
        if let Some(existing_source) = self.by_key.get(&key).copied() {
            let existing_fingerprint = self
                .by_id
                .get(&existing_source)
                .ok_or(CatalogError::UnknownSource(existing_source))?
                .fingerprint;
            if existing_fingerprint != fingerprint {
                return Err(CatalogError::FingerprintConflict {
                    source_id: existing_source,
                    expected: Box::new(existing_fingerprint),
                    actual: Box::new(fingerprint),
                });
            }
            if matches!(&descriptor.placement, SourcePlacement::Root { .. }) {
                let existing_origin = &self
                    .by_id
                    .get(&existing_source)
                    .ok_or(CatalogError::UnknownSource(existing_source))?
                    .physical_origin;
                if existing_origin != &physical_origin {
                    return Err(CatalogError::PhysicalOriginChanged {
                        source_id: existing_source,
                        expected: Box::new(existing_origin.clone()),
                        actual: Box::new(physical_origin),
                    });
                }
            }
            return Ok(existing_source);
        }

        let source = SourceId::new(
            self.workspace,
            descriptor.kind,
            deterministic_local_id(&key),
        )
        .map_err(|error| CatalogError::InvalidIdentity(error.to_string()))?;
        if let Some(existing) = self.by_id.get(&source) {
            return Err(CatalogError::IdentityCollision {
                existing: Box::new(existing.source_locator.clone()),
                incoming: Box::new(source_locator),
            });
        }
        if let Some(existing) = self.by_locator.get(&source_locator) {
            return Err(CatalogError::LocatorCollision {
                existing: *existing,
                incoming: source,
            });
        }
        if matches!(&descriptor.placement, SourcePlacement::Root { .. }) {
            self.ensure_physical_available(source, &physical_origin)?;
            self.physical_roots.insert(physical_origin.clone(), source);
        }

        self.by_key.insert(key.clone(), source);
        self.by_locator.insert(source_locator.clone(), source);
        self.by_id.insert(
            source,
            SourceRecord {
                descriptor,
                fingerprint,
                source_locator,
                physical_origin,
                canonical_key: key,
            },
        );
        Ok(source)
    }

    pub fn resolve(&self, source: SourceId) -> Result<&SourceDescriptor, CatalogError> {
        self.ensure_workspace(source)?;
        self.by_id
            .get(&source)
            .map(|record| &record.descriptor)
            .ok_or(CatalogError::UnknownSource(source))
    }

    pub fn source_locator(&self, source: SourceId) -> Result<&SourceLocator, CatalogError> {
        self.ensure_workspace(source)?;
        self.by_id
            .get(&source)
            .map(|record| &record.source_locator)
            .ok_or(CatalogError::UnknownSource(source))
    }

    pub fn physical_origin(&self, source: SourceId) -> Result<&PhysicalOrigin, CatalogError> {
        self.ensure_workspace(source)?;
        self.by_id
            .get(&source)
            .map(|record| &record.physical_origin)
            .ok_or(CatalogError::UnknownSource(source))
    }

    pub fn lookup_physical(&self, origin: &PhysicalOrigin) -> Result<SourceId, CatalogError> {
        self.physical_roots
            .get(origin)
            .copied()
            .ok_or_else(|| CatalogError::UnknownPhysicalOrigin(origin.clone()))
    }

    pub fn resolve_object_address(
        &self,
        address: &ObjectAddress,
    ) -> Result<RevisionedObjectHandle, CatalogError> {
        let object = self.resolve_object_id(address)?;
        RevisionedObjectHandle::new(self.workspace, self.revision()?, object)
            .map_err(|error| CatalogError::InvalidIdentity(error.to_string()))
    }

    fn resolve_object_id(&self, address: &ObjectAddress) -> Result<ObjectId, CatalogError> {
        let source_kind = match address.kind() {
            ObjectKind::Binary => SourceKind::SerializedFile,
            ObjectKind::Yaml => SourceKind::Yaml,
        };
        let source = self
            .by_locator
            .get(address.source_locator())
            .copied()
            .ok_or_else(|| CatalogError::UnknownObjectAddress(Box::new(address.clone())))?;
        if source.kind() != source_kind {
            return Err(CatalogError::ObjectAddressSourceKindMismatch {
                address: Box::new(address.clone()),
                expected: source_kind,
                actual: source.kind(),
            });
        }
        match address.kind() {
            ObjectKind::Binary => ObjectId::binary(
                source,
                address
                    .binary_path_id()
                    .ok_or_else(|| CatalogError::UnknownObjectAddress(Box::new(address.clone())))?,
            ),
            ObjectKind::Yaml => ObjectId::from_yaml_selector(
                source,
                address
                    .yaml_selector()
                    .ok_or_else(|| CatalogError::UnknownObjectAddress(Box::new(address.clone())))?,
            ),
        }
        .map_err(|error| CatalogError::InvalidIdentity(error.to_string()))
    }

    pub fn address_for_handle(
        &self,
        handle: &RevisionedObjectHandle,
    ) -> Result<ObjectAddress, CatalogError> {
        handle
            .validate_context(self.workspace, self.revision()?)
            .map_err(|error| CatalogError::InvalidHandleContext(error.to_string()))?;
        self.address_for_object(handle.object())
    }

    fn address_for_object(&self, object: &ObjectId) -> Result<ObjectAddress, CatalogError> {
        self.ensure_workspace(object.source())?;
        let source_locator = self.source_locator(object.source())?.clone();
        match object.kind() {
            ObjectKind::Binary => ObjectAddress::binary_at(
                source_locator,
                object
                    .binary_path_id()
                    .ok_or_else(|| CatalogError::UnknownObject(Box::new(object.clone())))?,
            ),
            ObjectKind::Yaml => {
                if let Some(anchor) = object.yaml_anchor() {
                    ObjectAddress::yaml(source_locator, anchor)
                } else if let Some(index) = object.yaml_document_ordinal() {
                    ObjectAddress::yaml_document(source_locator, index)
                } else {
                    return Err(CatalogError::UnknownObject(Box::new(object.clone())));
                }
            }
        }
        .map_err(|error| CatalogError::InvalidIdentity(error.to_string()))
    }

    pub fn update_fingerprint(
        &mut self,
        source: SourceId,
        fingerprint: SourceFingerprint,
    ) -> Result<(), CatalogError> {
        self.ensure_workspace(source)?;
        if source.kind() != fingerprint.kind() {
            return Err(CatalogError::SourceKindMismatch {
                expected: source.kind(),
                actual: fingerprint.kind(),
            });
        }
        let record = self
            .by_id
            .get_mut(&source)
            .ok_or(CatalogError::UnknownSource(source))?;
        record.fingerprint = fingerprint;
        Ok(())
    }

    pub fn fingerprint(&self, source: SourceId) -> Result<SourceFingerprint, CatalogError> {
        self.ensure_workspace(source)?;
        self.by_id
            .get(&source)
            .map(|record| record.fingerprint)
            .ok_or(CatalogError::UnknownSource(source))
    }

    pub fn revision(&self) -> Result<WorkspaceRevision, CatalogError> {
        const PREFIX: &[u8] = b"unity-asset:source-catalog:v2\0";

        let mut logical_length = checked_len(PREFIX.len())?;
        logical_length = checked_add(logical_length, 16)?;
        for (source, record) in &self.by_id {
            logical_length = checked_add(logical_length, 16)?;
            logical_length = checked_add(logical_length, 8)?;
            logical_length = checked_add(logical_length, checked_len(source.kind().tag().len())?)?;
            logical_length = checked_add(logical_length, 8)?;
            logical_length = checked_add(logical_length, checked_len(record.canonical_key.len())?)?;
            logical_length = checked_add(logical_length, DigestV1::BYTE_LEN as u64)?;
        }

        let mut digest = DigestV1Builder::new(logical_length);
        digest.update(PREFIX)?;
        digest.update(&self.workspace.get().to_le_bytes())?;
        for (source, record) in &self.by_id {
            digest.update(&source.local().to_le_bytes())?;
            update_framed(&mut digest, source.kind().tag().as_bytes())?;
            update_framed(&mut digest, &record.canonical_key)?;
            digest.update(record.fingerprint.digest().as_bytes())?;
        }
        Ok(WorkspaceRevision::new(digest.finalize()?))
    }

    #[must_use]
    pub const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (SourceId, &SourceDescriptor)> {
        self.by_id
            .iter()
            .map(|(source, record)| (*source, &record.descriptor))
    }

    fn resolve_placement(
        &self,
        descriptor: &SourceDescriptor,
    ) -> Result<(SourceLocator, PhysicalOrigin), CatalogError> {
        match &descriptor.placement {
            SourcePlacement::Root {
                alias,
                physical_origin,
            } => Ok((
                SourceLocator::path(alias.as_str())
                    .map_err(|error| CatalogError::InvalidIdentity(error.to_string()))?,
                physical_origin.clone(),
            )),
            SourcePlacement::Member { parent, step, .. } => {
                self.ensure_workspace(*parent)?;
                let parent_record = self
                    .by_id
                    .get(parent)
                    .ok_or(CatalogError::UnknownSource(*parent))?;
                let source_locator = parent_record
                    .source_locator
                    .clone()
                    .child(step.container(), step.member().clone())
                    .map_err(|error| CatalogError::InvalidIdentity(error.to_string()))?;
                Ok((source_locator, parent_record.physical_origin.clone()))
            }
        }
    }

    fn ensure_workspace(&self, source: SourceId) -> Result<(), CatalogError> {
        if source.workspace() != self.workspace {
            return Err(CatalogError::WorkspaceMismatch {
                expected: self.workspace,
                actual: source.workspace(),
            });
        }
        Ok(())
    }

    fn ensure_physical_available(
        &self,
        source: SourceId,
        origin: &PhysicalOrigin,
    ) -> Result<(), CatalogError> {
        if let Some(existing) = self.physical_roots.get(origin)
            && *existing != source
        {
            return Err(CatalogError::PhysicalOriginConflict {
                origin: Box::new(origin.clone()),
                existing: Box::new(*existing),
                incoming: Box::new(source),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CatalogError {
    #[error("invalid source identity: {0}")]
    InvalidIdentity(String),
    #[error("invalid revisioned object handle: {0}")]
    InvalidHandleContext(String),
    #[error("streamed resources must be declared through SourceDescriptor::sidecar")]
    StreamedResourceRequiresSidecar,
    #[error("source catalog revision input length overflow")]
    RevisionLengthOverflow,
    #[error(transparent)]
    RevisionDigest(#[from] DigestBuildError),
    #[error("source kind mismatch: expected {expected:?}, got {actual:?}")]
    SourceKindMismatch {
        expected: SourceKind,
        actual: SourceKind,
    },
    #[error("source {parent:?} must be {expected:?}, got {actual:?}")]
    InvalidParentKind {
        parent: SourceId,
        expected: SourceKind,
        actual: SourceKind,
    },
    #[error("sidecar parent {parent:?} is not a container source: {actual:?}")]
    InvalidSidecarParentKind {
        parent: SourceId,
        actual: SourceKind,
    },
    #[error("deterministic source identity collision between {existing:?} and {incoming:?}")]
    IdentityCollision {
        existing: Box<SourceLocator>,
        incoming: Box<SourceLocator>,
    },
    #[error("source locator collision between {existing:?} and {incoming:?}")]
    LocatorCollision {
        existing: SourceId,
        incoming: SourceId,
    },
    #[error("source {source_id:?} fingerprint changed during registration")]
    FingerprintConflict {
        source_id: SourceId,
        expected: Box<SourceFingerprint>,
        actual: Box<SourceFingerprint>,
    },
    #[error("physical origin {origin:?} maps to both {existing:?} and {incoming:?}")]
    PhysicalOriginConflict {
        origin: Box<PhysicalOrigin>,
        existing: Box<SourceId>,
        incoming: Box<SourceId>,
    },
    #[error("source {source_id:?} is already bound to {expected:?}, not {actual:?}")]
    PhysicalOriginChanged {
        source_id: SourceId,
        expected: Box<PhysicalOrigin>,
        actual: Box<PhysicalOrigin>,
    },
    #[error("source belongs to workspace {actual}, not {expected}")]
    WorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("unknown source id: {0:?}")]
    UnknownSource(SourceId),
    #[error("unknown physical origin: {0:?}")]
    UnknownPhysicalOrigin(PhysicalOrigin),
    #[error("object address does not resolve in this catalog: {0:?}")]
    UnknownObjectAddress(Box<ObjectAddress>),
    #[error(
        "object address {address:?} requires source kind {expected:?}, but its locator owns {actual:?}"
    )]
    ObjectAddressSourceKindMismatch {
        address: Box<ObjectAddress>,
        expected: SourceKind,
        actual: SourceKind,
    },
    #[error("object does not resolve in this catalog: {0:?}")]
    UnknownObject(Box<ObjectId>),
}

fn ensure_regular_member_kind(kind: SourceKind) -> Result<(), CatalogError> {
    if kind == SourceKind::StreamedResource {
        Err(CatalogError::StreamedResourceRequiresSidecar)
    } else {
        Ok(())
    }
}

fn canonical_source_key(kind: SourceKind, locator: &SourceLocator) -> Vec<u8> {
    let mut key = Vec::new();
    key.extend_from_slice(b"unity-asset:source:v2\0");
    append_bytes(&mut key, kind.tag().as_bytes());
    append_bytes(&mut key, locator.root_alias().as_str().as_bytes());
    key.extend_from_slice(&(locator.members().len() as u64).to_le_bytes());
    for step in locator.members() {
        append_bytes(&mut key, step.container().tag().as_bytes());
        append_bytes(&mut key, step.member().name().as_bytes());
        key.extend_from_slice(&step.member().same_name_occurrence().to_le_bytes());
    }
    key
}

fn append_bytes(target: &mut Vec<u8>, bytes: &[u8]) {
    target.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    target.extend_from_slice(bytes);
}

fn checked_len(length: usize) -> Result<u64, CatalogError> {
    u64::try_from(length).map_err(|_| CatalogError::RevisionLengthOverflow)
}

fn checked_add(left: u64, right: u64) -> Result<u64, CatalogError> {
    left.checked_add(right)
        .ok_or(CatalogError::RevisionLengthOverflow)
}

fn update_framed(digest: &mut DigestV1Builder, bytes: &[u8]) -> Result<(), DigestBuildError> {
    let length = u64::try_from(bytes.len()).map_err(|_| DigestBuildError::LengthOverflow)?;
    digest.update(&length.to_le_bytes())?;
    digest.update(bytes)
}

fn deterministic_local_id(key: &[u8]) -> u128 {
    let digest = DigestV1::hash_bytes(key);
    let mut prefix = [0_u8; 16];
    prefix.copy_from_slice(&digest.as_bytes()[..16]);
    u128::from_le_bytes(prefix).max(1)
}
