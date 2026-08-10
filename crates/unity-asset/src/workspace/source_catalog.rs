use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BundleMemberId, ContainmentKind, ContainmentStep, ContractError,
    DigestBuildError, DigestV1, DigestV1Builder, ObjectAddress, ObjectId, ObjectKind, SourceAlias,
    SourceFingerprint, SourceId, SourceKind, SourceLocator, SourceMemberId, WorkspaceId,
    WorkspaceRevision, arc_value_allocation_bytes, vec_allocation_bytes,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Runtime filesystem binding. It is never serialized into a logical object address.
pub(crate) struct PhysicalOrigin(PathBuf);

impl PhysicalOrigin {
    /// Mints a canonical destination already proven by the publication protocol.
    ///
    /// The caller owns the path allocation and has verified its anchored parent. The destination
    /// may still be absent because publication stages and proves bytes before the atomic move.
    pub(crate) fn from_verified_publication_path(
        path: PathBuf,
    ) -> Result<Self, PhysicalOriginError> {
        if !path.is_absolute() {
            return Err(PhysicalOriginError::NotAbsolute(path));
        }
        #[cfg(windows)]
        validate_windows_origin_path(&path)?;
        Ok(Self(path))
    }

    #[cfg(test)]
    pub(crate) fn from_existing_path(path: impl AsRef<Path>) -> Result<Self, PhysicalOriginError> {
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
        Self::from_canonical_path(canonical)
    }

    pub(crate) fn from_existing_path_budgeted(
        path: impl AsRef<Path>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, CatalogError> {
        let canonical = BudgetedCanonicalPath::resolve(path.as_ref(), budget)?;
        let retained_bytes = canonical.planned_bytes();
        let origin = Self::from_canonical_path(canonical.into_path())?;
        budget.consume_bytes(retained_bytes)?;
        Ok(origin)
    }

    fn from_canonical_path(canonical: PathBuf) -> Result<Self, PhysicalOriginError> {
        let metadata =
            fs::metadata(&canonical).map_err(|error| PhysicalOriginError::io(&canonical, error))?;
        if !metadata.is_file() {
            return Err(PhysicalOriginError::NotRegularFile(canonical));
        }
        Ok(Self(canonical))
    }

    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    #[must_use]
    pub(crate) fn retained_owned_bytes(&self) -> usize {
        self.0.capacity()
    }

    #[must_use]
    pub(crate) fn into_path(self) -> PathBuf {
        self.0
    }

    pub(crate) fn try_clone_for_index(&self) -> Result<Self, std::collections::TryReserveError> {
        let mut path = PathBuf::new();
        path.as_mut_os_string()
            .try_reserve(self.0.as_os_str().len())?;
        path.push(&self.0);
        Ok(Self(path))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum PhysicalOriginError {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
/// Placement shape exposed for structured source inspection.
pub enum SourceLocationKind {
    Root,
    ArchiveMember,
    WebFileMember,
    BundleMember,
    Sidecar,
    Companion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Validated source declaration consumed by `SourceCatalog::register`.
pub(crate) struct SourceDescriptor {
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
    Companion {
        parent: SourceId,
        step: ContainmentStep,
    },
}

impl SourceDescriptor {
    #[must_use]
    pub(crate) fn root(
        kind: SourceKind,
        alias: SourceAlias,
        physical_origin: PhysicalOrigin,
    ) -> Self {
        Self {
            kind,
            placement: SourcePlacement::Root {
                alias,
                physical_origin,
            },
        }
    }

    pub(crate) fn archive_member(
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

    pub(crate) fn webfile_member(
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

    pub(crate) fn bundle_member(
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

    pub(crate) fn sidecar(parent: SourceId, member: SourceMemberId) -> Result<Self, CatalogError> {
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

    pub(crate) fn companion(
        parent: SourceId,
        member: SourceMemberId,
    ) -> Result<Self, CatalogError> {
        if !supports_companion(parent.kind()) {
            return Err(CatalogError::InvalidCompanionParentKind {
                parent,
                actual: parent.kind(),
            });
        }
        Ok(Self {
            kind: SourceKind::StreamedResource,
            placement: SourcePlacement::Companion {
                parent,
                step: ContainmentStep::new(ContainmentKind::Companion, member),
            },
        })
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
    pub(crate) const fn kind(&self) -> SourceKind {
        self.kind
    }

    #[must_use]
    pub(crate) fn parent(&self) -> Option<SourceId> {
        match &self.placement {
            SourcePlacement::Root { .. } => None,
            SourcePlacement::Member { parent, .. } | SourcePlacement::Companion { parent, .. } => {
                Some(*parent)
            }
        }
    }

    #[must_use]
    pub(crate) fn location_kind(&self) -> SourceLocationKind {
        match &self.placement {
            SourcePlacement::Root { .. } => SourceLocationKind::Root,
            SourcePlacement::Member { location_kind, .. } => *location_kind,
            SourcePlacement::Companion { .. } => SourceLocationKind::Companion,
        }
    }

    #[must_use]
    fn has_independent_physical_origin(&self) -> bool {
        matches!(
            &self.placement,
            SourcePlacement::Root { .. } | SourcePlacement::Companion { .. }
        )
    }

    #[must_use]
    fn child_step(&self) -> Option<(SourceId, &ContainmentStep)> {
        match &self.placement {
            SourcePlacement::Root { .. } => None,
            SourcePlacement::Member { parent, step, .. }
            | SourcePlacement::Companion { parent, step, .. } => Some((*parent, step)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// One existing source fingerprint replacement requested by a prepared artifact graph.
pub(crate) struct PhysicalDomainChange {
    source: SourceId,
    replacement: SourceFingerprint,
}

impl PhysicalDomainChange {
    #[must_use]
    pub(crate) const fn new(source: SourceId, replacement: SourceFingerprint) -> Self {
        Self {
            source,
            replacement,
        }
    }

    #[must_use]
    pub(crate) const fn source(&self) -> SourceId {
        self.source
    }

    #[must_use]
    pub(crate) const fn replacement(&self) -> SourceFingerprint {
        self.replacement
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ObservedPhysicalDomainSource {
    owner: SourceId,
    source: SourceId,
    fingerprint: SourceFingerprint,
}

#[derive(Debug)]
/// Complete multi-domain CAS observation bound to an ordered replacement set.
pub(crate) struct PhysicalDomainRewriteBatch<'change> {
    workspace: WorkspaceId,
    owners: Vec<SourceId>,
    observed: Vec<ObservedPhysicalDomainSource>,
    changes: &'change [PhysicalDomainChange],
    #[cfg(test)]
    owner_resolutions: usize,
}

#[derive(Debug)]
struct PreparedPhysicalDomainChange {
    source: SourceId,
    record: Arc<SourceRecord>,
}

#[derive(Debug)]
struct SourceRecord {
    descriptor: SourceDescriptor,
    fingerprint: SourceFingerprint,
    source_locator: Arc<SourceLocator>,
    physical_origin: Option<Arc<PhysicalOrigin>>,
    canonical_key: Arc<Vec<u8>>,
}

#[derive(Debug)]
struct SubtreeRemoval {
    sources: Vec<SourceId>,
    #[cfg(test)]
    index_visits: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhysicalFileIdentity {
    length: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial_number: u64,
    #[cfg(windows)]
    file_id: [u8; 16],
}

impl PhysicalFileIdentity {
    #[must_use]
    pub(crate) const fn length(&self) -> u64 {
        self.length
    }

    #[cfg(unix)]
    #[must_use]
    pub(crate) const fn unix_parts(&self) -> (u64, u64) {
        (self.device, self.inode)
    }

    #[cfg(windows)]
    #[must_use]
    pub(crate) const fn windows_parts(&self) -> (u64, [u8; 16]) {
        (self.volume_serial_number, self.file_id)
    }
}

/// Canonical path observation whose budget charge is committed only after its proof succeeds.
#[derive(Debug)]
struct BudgetedCanonicalPath {
    path: PathBuf,
    planned_bytes: u64,
}

impl BudgetedCanonicalPath {
    fn resolve(requested: &Path, budget: &AssetLoadBudget) -> Result<Self, CatalogError> {
        if !requested.is_absolute() {
            return Err(PhysicalOriginError::NotAbsolute(requested.to_path_buf()).into());
        }
        #[cfg(windows)]
        validate_windows_origin_path(requested)?;

        let requested_bytes = checked_usize_to_u64(requested.as_os_str().len())?;
        budget.check_bytes(requested_bytes)?;
        let path = fs::canonicalize(requested)
            .map_err(|error| CatalogError::verified_binding_io(requested, error))?;
        #[cfg(windows)]
        validate_windows_origin_path(&path)?;
        Self::from_path(path, requested_bytes, budget)
    }

    fn from_path(
        path: PathBuf,
        minimum_bytes: u64,
        budget: &AssetLoadBudget,
    ) -> Result<Self, CatalogError> {
        let allocation_bytes = checked_usize_to_u64(path.capacity())?;
        let planned_bytes = allocation_bytes.max(minimum_bytes);
        budget.check_bytes(planned_bytes)?;
        Ok(Self {
            path,
            planned_bytes,
        })
    }

    #[must_use]
    fn path(&self) -> &Path {
        self.path.as_path()
    }

    #[must_use]
    const fn planned_bytes(&self) -> u64 {
        self.planned_bytes
    }

    #[must_use]
    fn into_path(self) -> PathBuf {
        self.path
    }
}

/// Proof that one canonical regular file matched an expected source fingerprint.
#[derive(Debug)]
pub(crate) struct VerifiedPhysicalBinding {
    kind: SourceKind,
    physical_origin: PhysicalOrigin,
    fingerprint: SourceFingerprint,
    file_identity: PhysicalFileIdentity,
}

impl VerifiedPhysicalBinding {
    pub(crate) fn verify_existing(
        kind: SourceKind,
        path: impl AsRef<Path>,
        expected_fingerprint: SourceFingerprint,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, CatalogError> {
        Self::observe_existing_impl(kind, path.as_ref(), Some(expected_fingerprint), budget)
    }

    pub(crate) fn observe_existing(
        kind: SourceKind,
        path: impl AsRef<Path>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, CatalogError> {
        Self::observe_existing_impl(kind, path.as_ref(), None, budget)
    }

    fn observe_existing_impl(
        kind: SourceKind,
        requested: &Path,
        expected_fingerprint: Option<SourceFingerprint>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, CatalogError> {
        if let Some(expected_fingerprint) = expected_fingerprint
            && expected_fingerprint.kind() != kind
        {
            return Err(CatalogError::SourceKindMismatch {
                expected: kind,
                actual: expected_fingerprint.kind(),
            });
        }
        let canonical = BudgetedCanonicalPath::resolve(requested, budget)?;
        let canonical_path_bytes = canonical.planned_bytes();
        let physical_origin = PhysicalOrigin::from_canonical_path(canonical.into_path())?;
        let mut file = open_verified_file(physical_origin.path())
            .map_err(|error| CatalogError::verified_binding_io(physical_origin.path(), error))?;
        let before = physical_file_identity(&file, physical_origin.path())?;
        let planned_bytes = checked_byte_add(before.length, canonical_path_bytes)?;
        budget.check_bytes(planned_bytes)?;

        let digest = DigestV1::hash_reader(&mut file, before.length)
            .map_err(|error| CatalogError::verified_binding_io(physical_origin.path(), error))?;
        let after = physical_file_identity(&file, physical_origin.path())?;
        let path_identity = physical_file_identity_from_path(physical_origin.path())?;
        if before != after || before != path_identity {
            return Err(CatalogError::VerifiedPhysicalBindingChanged {
                path: physical_origin.path().to_path_buf(),
            });
        }

        let fingerprint = SourceFingerprint::new(kind, digest);
        if let Some(expected_fingerprint) = expected_fingerprint
            && fingerprint != expected_fingerprint
        {
            return Err(CatalogError::VerifiedFingerprintMismatch {
                expected: expected_fingerprint,
                actual: fingerprint,
            });
        }
        budget.consume_bytes(planned_bytes)?;
        Ok(Self {
            kind,
            physical_origin,
            fingerprint,
            file_identity: before,
        })
    }

    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        self.physical_origin.path()
    }

    #[must_use]
    pub(crate) const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }

    #[must_use]
    pub(crate) const fn file_identity(&self) -> &PhysicalFileIdentity {
        &self.file_identity
    }

    const fn revalidation_bytes(&self) -> u64 {
        self.file_identity.length
    }

    pub(crate) fn revalidate_current_contents(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), CatalogError> {
        let planned_bytes = self.revalidation_bytes();
        budget.check_bytes(planned_bytes)?;
        revalidate_physical_contents(
            self.kind,
            &self.physical_origin,
            self.fingerprint,
            &self.file_identity,
        )?;
        budget.consume_bytes(planned_bytes)?;
        Ok(())
    }
}

/// Stable identity proof for a canonical directory used as an absent target's parent.
#[derive(Debug)]
pub(crate) struct VerifiedPhysicalDirectoryBinding {
    path: PathBuf,
    identity: PhysicalFileIdentity,
}

impl VerifiedPhysicalDirectoryBinding {
    pub(crate) fn verify_existing(
        requested: impl AsRef<Path>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, CatalogError> {
        let requested = requested.as_ref();
        if !requested.is_absolute() {
            return Err(PhysicalOriginError::NotAbsolute(requested.to_path_buf()).into());
        }
        #[cfg(windows)]
        validate_windows_origin_path(requested)?;
        budget.check_entries(1)?;

        let canonical = BudgetedCanonicalPath::resolve(requested, budget)?;
        let planned_bytes = canonical.planned_bytes();
        let file = open_verified_directory(canonical.path())
            .map_err(|error| CatalogError::verified_binding_io(canonical.path(), error))?;
        let before = physical_directory_identity(&file, canonical.path())?;
        let after = physical_directory_identity(&file, canonical.path())?;
        let path_identity = physical_directory_identity_from_path(canonical.path())?;
        if before != after || before != path_identity || path_is_symlink(canonical.path())? {
            return Err(CatalogError::VerifiedPhysicalBindingChanged {
                path: canonical.into_path(),
            });
        }

        budget.consume_entries(1)?;
        budget.consume_bytes(planned_bytes)?;
        Ok(Self {
            path: canonical.into_path(),
            identity: before,
        })
    }

    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub(crate) const fn identity(&self) -> &PhysicalFileIdentity {
        &self.identity
    }

    pub(crate) fn revalidate_current_entry(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), CatalogError> {
        budget.check_entries(1)?;
        let canonical = BudgetedCanonicalPath::resolve(&self.path, budget)?;
        let planned_bytes = canonical.planned_bytes();
        let file = open_verified_directory(&self.path)
            .map_err(|error| CatalogError::verified_binding_io(&self.path, error))?;
        let before = physical_directory_identity(&file, &self.path)?;
        let after = physical_directory_identity(&file, &self.path)?;
        let path_identity = physical_directory_identity_from_path(&self.path)?;
        if canonical.path() != self.path.as_path()
            || before != self.identity
            || before != after
            || before != path_identity
            || path_is_symlink(&self.path)?
        {
            return Err(CatalogError::VerifiedPhysicalBindingChanged {
                path: self.path.clone(),
            });
        }
        budget.consume_entries(1)?;
        budget.consume_bytes(planned_bytes)?;
        Ok(())
    }
}

fn revalidate_physical_contents(
    kind: SourceKind,
    physical_origin: &PhysicalOrigin,
    fingerprint: SourceFingerprint,
    file_identity: &PhysicalFileIdentity,
) -> Result<(), CatalogError> {
    if path_is_symlink(physical_origin.path())? {
        return Err(CatalogError::VerifiedPhysicalBindingChanged {
            path: physical_origin.path().to_path_buf(),
        });
    }
    let mut file = open_verified_file(physical_origin.path())
        .map_err(|error| CatalogError::verified_binding_io(physical_origin.path(), error))?;
    let before = physical_file_identity(&file, physical_origin.path())?;
    if before.length != file_identity.length {
        return Err(CatalogError::VerifiedPhysicalBindingChanged {
            path: physical_origin.path().to_path_buf(),
        });
    }

    let digest = DigestV1::hash_reader(&mut file, before.length)
        .map_err(|error| CatalogError::verified_binding_io(physical_origin.path(), error))?;
    let after = physical_file_identity(&file, physical_origin.path())?;
    let path_identity = physical_file_identity_from_path(physical_origin.path())?;
    if before != after || before != path_identity {
        return Err(CatalogError::VerifiedPhysicalBindingChanged {
            path: physical_origin.path().to_path_buf(),
        });
    }

    let actual = SourceFingerprint::new(kind, digest);
    if actual != fingerprint {
        return Err(CatalogError::VerifiedFingerprintMismatch {
            expected: fingerprint,
            actual,
        });
    }
    if &before != file_identity {
        return Err(CatalogError::VerifiedPhysicalBindingChanged {
            path: physical_origin.path().to_path_buf(),
        });
    }
    Ok(())
}

pub(super) fn physical_file_identity(
    file: &fs::File,
    path: &Path,
) -> Result<PhysicalFileIdentity, CatalogError> {
    physical_node_identity(file, path, PhysicalNodeKind::File)
}

fn physical_directory_identity(
    file: &fs::File,
    path: &Path,
) -> Result<PhysicalFileIdentity, CatalogError> {
    physical_node_identity(file, path, PhysicalNodeKind::Directory)
}

#[derive(Clone, Copy)]
enum PhysicalNodeKind {
    File,
    Directory,
}

fn physical_node_identity(
    file: &fs::File,
    path: &Path,
    expected_kind: PhysicalNodeKind,
) -> Result<PhysicalFileIdentity, CatalogError> {
    let metadata = file
        .metadata()
        .map_err(|error| CatalogError::verified_binding_io(path, error))?;
    let kind_matches = match expected_kind {
        PhysicalNodeKind::File => metadata.is_file(),
        PhysicalNodeKind::Directory => metadata.is_dir(),
    };
    if !kind_matches {
        return Err(CatalogError::VerifiedPhysicalBindingChanged {
            path: path.to_path_buf(),
        });
    }
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;
    #[cfg(windows)]
    let (volume_serial_number, file_id) = windows_file_identity(file, path)?;
    Ok(PhysicalFileIdentity {
        // Directory entry timestamps and directory size legitimately change
        // when the journal creates staging/recovery children. Only a regular
        // file's length participates in content identity; directory identity
        // is the stable node id below.
        length: match expected_kind {
            PhysicalNodeKind::File => metadata.len(),
            PhysicalNodeKind::Directory => 0,
        },
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(windows)]
        volume_serial_number,
        #[cfg(windows)]
        file_id,
    })
}

pub(super) fn physical_file_identity_from_path(
    path: &Path,
) -> Result<PhysicalFileIdentity, CatalogError> {
    let file =
        open_verified_file(path).map_err(|error| CatalogError::verified_binding_io(path, error))?;
    physical_file_identity(&file, path)
}

fn physical_directory_identity_from_path(
    path: &Path,
) -> Result<PhysicalFileIdentity, CatalogError> {
    let file = open_verified_directory(path)
        .map_err(|error| CatalogError::verified_binding_io(path, error))?;
    physical_directory_identity(&file, path)
}

fn path_is_symlink(path: &Path) -> Result<bool, CatalogError> {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .map_err(|error| CatalogError::verified_binding_io(path, error))
}

pub(super) fn open_verified_file(path: &Path) -> io::Result<fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        fs::File::open(path)
    }
}

fn open_verified_directory(path: &Path) -> io::Result<fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };

        fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        fs::File::open(path)
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &fs::File, path: &Path) -> Result<(u64, [u8; 16]), CatalogError> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    let mut information = MaybeUninit::<FILE_ID_INFO>::zeroed();
    let information_size = u32::try_from(size_of::<FILE_ID_INFO>()).map_err(|_| {
        CatalogError::AllocationSizeOverflow {
            resource: "Windows file identity",
        }
    })?;
    // SAFETY: `file` owns a valid handle for the duration of the call and `information` points to
    // writable storage for the exact structure required by FileIdInfo.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            information.as_mut_ptr().cast(),
            information_size,
        )
    };
    if succeeded == 0 {
        return Err(CatalogError::verified_binding_io(
            path,
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: a nonzero return guarantees that Windows initialized the output structure.
    let information = unsafe { information.assume_init() };
    Ok((
        information.VolumeSerialNumber,
        information.FileId.Identifier,
    ))
}

/// Workspace-local authority for source ownership, physical bindings, and opaque identities.
#[derive(Debug)]
pub(crate) struct SourceCatalog {
    workspace: WorkspaceId,
    by_key: HashMap<Arc<Vec<u8>>, SourceId>,
    by_id: BTreeMap<SourceId, Arc<SourceRecord>>,
    by_locator: HashMap<Arc<SourceLocator>, SourceId>,
    physical_bindings: HashMap<Arc<PhysicalOrigin>, SourceId>,
    root_aliases: HashMap<Arc<SourceAlias>, SourceId>,
    children_by_parent: HashMap<SourceId, HashMap<Arc<ContainmentStep>, SourceId>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocatorResolution {
    Resolved(SourceId),
    Unloaded,
    Missing,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootAdmissionDecision {
    Vacant,
    Unchanged(SourceId),
    AliasConflict { existing: SourceId },
    PhysicalOriginConflict { existing: SourceId },
}

impl SourceCatalog {
    #[must_use]
    pub(crate) fn new(workspace: WorkspaceId) -> Self {
        Self {
            workspace,
            by_key: HashMap::new(),
            by_id: BTreeMap::new(),
            by_locator: HashMap::new(),
            physical_bindings: HashMap::new(),
            root_aliases: HashMap::new(),
            children_by_parent: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn register(
        &mut self,
        descriptor: SourceDescriptor,
        fingerprint: SourceFingerprint,
    ) -> Result<SourceId, CatalogError> {
        self.register_impl(descriptor, fingerprint, None)
    }

    fn register_impl(
        &mut self,
        descriptor: SourceDescriptor,
        fingerprint: SourceFingerprint,
        budget: Option<&mut AssetLoadBudget>,
    ) -> Result<SourceId, CatalogError> {
        if descriptor.kind != fingerprint.kind() {
            return Err(CatalogError::SourceKindMismatch {
                expected: descriptor.kind,
                actual: fingerprint.kind(),
            });
        }

        if let Some(existing_source) = self.existing_source_for_descriptor(&descriptor)? {
            let existing_fingerprint = self
                .by_id
                .get(&existing_source)
                .ok_or(CatalogError::UnknownSource(existing_source))?
                .fingerprint;
            if existing_fingerprint != fingerprint {
                return Err(CatalogError::FingerprintConflict {
                    source_id: existing_source,
                    expected: existing_fingerprint,
                    actual: fingerprint,
                });
            }
            if descriptor.has_independent_physical_origin()
                && let SourcePlacement::Root {
                    physical_origin, ..
                } = &descriptor.placement
            {
                let existing_origin = &self
                    .by_id
                    .get(&existing_source)
                    .ok_or(CatalogError::UnknownSource(existing_source))?
                    .physical_origin;
                if existing_origin.as_deref() != Some(physical_origin) {
                    return Err(CatalogError::PhysicalOriginChanged {
                        source_id: existing_source,
                    });
                }
            }
            return Ok(existing_source);
        }

        let retained_bytes = self.checked_registration_bytes(&descriptor)?;
        if let Some(budget) = budget.as_deref() {
            budget.check_entries(1)?;
            budget.check_bytes(retained_bytes)?;
        }

        let (source_locator, physical_origin) = self.resolve_placement(&descriptor)?;
        let key = canonical_source_key(descriptor.kind, &source_locator)?;

        let source = SourceId::new(
            self.workspace,
            descriptor.kind,
            deterministic_local_id(&key),
        )
        .map_err(CatalogError::InvalidIdentity)?;
        if let Some(existing) = self.by_id.get(&source) {
            return Err(CatalogError::IdentityCollision {
                source_id: source,
                existing_kind: existing.descriptor.kind,
            });
        }
        if let Some(existing) = self.by_locator.get(&source_locator) {
            return Err(CatalogError::LocatorCollision {
                existing: *existing,
                incoming: source,
            });
        }

        if descriptor.has_independent_physical_origin()
            && let Some(physical_origin) = &physical_origin
        {
            self.ensure_physical_available(source, physical_origin)?;
        }
        self.reserve_source(&descriptor)?;

        if let Some(budget) = budget {
            budget.consume_entries(1)?;
            budget.consume_bytes(retained_bytes)?;
        }

        let source_locator = Arc::new(source_locator);
        let key = Arc::new(key);
        if descriptor.has_independent_physical_origin()
            && let Some(physical_origin) = &physical_origin
        {
            self.physical_bindings
                .insert(physical_origin.clone(), source);
        }
        match &descriptor.placement {
            SourcePlacement::Root { alias, .. } => {
                self.root_aliases.insert(Arc::new(alias.clone()), source);
            }
            SourcePlacement::Member { parent, step, .. }
            | SourcePlacement::Companion { parent, step, .. } => {
                self.children_by_parent
                    .get_mut(parent)
                    .ok_or(CatalogError::InvariantMissingChildIndex { parent: *parent })?
                    .insert(Arc::new(step.clone()), source);
            }
        }

        self.by_key.insert(key.clone(), source);
        self.by_locator.insert(source_locator.clone(), source);
        self.by_id.insert(
            source,
            Arc::new(SourceRecord {
                descriptor,
                fingerprint,
                source_locator,
                physical_origin,
                canonical_key: key,
            }),
        );
        Ok(source)
    }

    fn bind_companion_origin_impl(
        &mut self,
        source: SourceId,
        origin: PhysicalOrigin,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), CatalogError> {
        self.ensure_workspace(source)?;
        let record = Arc::clone(
            self.by_id
                .get(&source)
                .ok_or(CatalogError::UnknownSource(source))?,
        );
        if !matches!(
            record.descriptor.placement,
            SourcePlacement::Companion { .. }
        ) {
            return Err(CatalogError::PhysicalOriginRequiresCompanion { source_id: source });
        }
        if let Some(existing) = &record.physical_origin {
            return if existing.as_ref() == &origin {
                Ok(())
            } else {
                Err(CatalogError::PhysicalOriginChanged { source_id: source })
            };
        }
        self.ensure_physical_available(source, &origin)?;

        let retained_bytes = checked_byte_add(
            checked_record_clone_bytes(&record.descriptor)?,
            checked_byte_add(
                checked_arc_allocation_bytes::<PhysicalOrigin>()?,
                checked_hash_map_growth_bytes(
                    &self.physical_bindings,
                    1,
                    "source catalog companion physical-binding index",
                )?,
            )?,
        )?;
        budget.check_bytes(retained_bytes)?;
        self.physical_bindings
            .try_reserve(1)
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog companion physical-binding index",
                requested: 1,
                unit: CatalogAllocationUnit::Slots,
                message: error.to_string(),
            })?;

        let origin = Arc::new(origin);
        let replacement = Arc::new(SourceRecord {
            descriptor: record.descriptor.clone(),
            fingerprint: record.fingerprint,
            source_locator: Arc::clone(&record.source_locator),
            physical_origin: Some(Arc::clone(&origin)),
            canonical_key: Arc::clone(&record.canonical_key),
        });
        budget.consume_bytes(retained_bytes)?;
        self.physical_bindings.insert(origin, source);
        self.by_id.insert(source, replacement);
        Ok(())
    }

    fn existing_source_for_descriptor(
        &self,
        descriptor: &SourceDescriptor,
    ) -> Result<Option<SourceId>, CatalogError> {
        let existing = match &descriptor.placement {
            SourcePlacement::Root { alias, .. } => self.root_aliases.get(alias).copied(),
            SourcePlacement::Member { parent, step, .. }
            | SourcePlacement::Companion { parent, step } => {
                self.ensure_workspace(*parent)?;
                self.children_by_parent
                    .get(parent)
                    .and_then(|children| children.get(step))
                    .copied()
            }
        };
        Ok(existing.filter(|source| source.kind() == descriptor.kind))
    }

    pub(crate) fn root_admission_decision(
        &self,
        alias: &SourceAlias,
        origin: &PhysicalOrigin,
        fingerprint: SourceFingerprint,
    ) -> Result<RootAdmissionDecision, CatalogError> {
        let alias_source = self.find_root_by_alias(alias);
        let origin_source = self.find_physical(origin);
        let Some(existing) = alias_source else {
            return Ok(match origin_source {
                Some(existing) => RootAdmissionDecision::PhysicalOriginConflict { existing },
                None => RootAdmissionDecision::Vacant,
            });
        };
        if self.fingerprint(existing)? != fingerprint {
            return Ok(RootAdmissionDecision::AliasConflict { existing });
        }
        if origin_source == Some(existing) {
            return Ok(RootAdmissionDecision::Unchanged(existing));
        }
        Ok(RootAdmissionDecision::PhysicalOriginConflict {
            existing: origin_source.unwrap_or(existing),
        })
    }

    fn checked_registration_bytes(
        &self,
        descriptor: &SourceDescriptor,
    ) -> Result<u64, CatalogError> {
        let placement_bytes = self.checked_placement_bytes(descriptor)?;
        let key_bytes =
            checked_usize_to_u64(self.canonical_source_key_len_for_descriptor(descriptor)?)?;
        checked_byte_add(
            checked_byte_add(placement_bytes, key_bytes)?,
            self.checked_source_storage_bytes(descriptor)?,
        )
    }

    fn canonical_source_key_len_for_descriptor(
        &self,
        descriptor: &SourceDescriptor,
    ) -> Result<usize, CatalogError> {
        match &descriptor.placement {
            SourcePlacement::Root { alias, .. } => {
                canonical_source_key_len_parts(descriptor.kind, alias, &[], None)
            }
            SourcePlacement::Member { parent, step, .. }
            | SourcePlacement::Companion { parent, step } => {
                self.ensure_workspace(*parent)?;
                let parent = self
                    .by_id
                    .get(parent)
                    .ok_or(CatalogError::UnknownSource(*parent))?;
                canonical_source_key_len_parts(
                    descriptor.kind,
                    parent.source_locator.root_alias(),
                    parent.source_locator.members(),
                    Some(step),
                )
            }
        }
    }

    fn reserve_source(&mut self, descriptor: &SourceDescriptor) -> Result<(), CatalogError> {
        self.by_key
            .try_reserve(1)
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog key index",
                requested: 1,
                unit: CatalogAllocationUnit::Slots,
                message: error.to_string(),
            })?;
        self.by_locator
            .try_reserve(1)
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog locator index",
                requested: 1,
                unit: CatalogAllocationUnit::Slots,
                message: error.to_string(),
            })?;
        if matches!(&descriptor.placement, SourcePlacement::Root { .. }) {
            self.physical_bindings.try_reserve(1).map_err(|error| {
                CatalogError::AllocationFailed {
                    resource: "source catalog physical-binding index",
                    requested: 1,
                    unit: CatalogAllocationUnit::Slots,
                    message: error.to_string(),
                }
            })?;
        }
        if matches!(&descriptor.placement, SourcePlacement::Root { .. }) {
            self.root_aliases
                .try_reserve(1)
                .map_err(|error| CatalogError::AllocationFailed {
                    resource: "source catalog root-alias index",
                    requested: 1,
                    unit: CatalogAllocationUnit::Slots,
                    message: error.to_string(),
                })?;
        }
        if let Some((parent, _)) = descriptor.child_step() {
            if !self.children_by_parent.contains_key(&parent) {
                self.children_by_parent.try_reserve(1).map_err(|error| {
                    CatalogError::AllocationFailed {
                        resource: "source catalog child index",
                        requested: 1,
                        unit: CatalogAllocationUnit::Slots,
                        message: error.to_string(),
                    }
                })?;
                let mut children = HashMap::new();
                children
                    .try_reserve(1)
                    .map_err(|error| CatalogError::AllocationFailed {
                        resource: "source catalog child-step index",
                        requested: 1,
                        unit: CatalogAllocationUnit::Slots,
                        message: error.to_string(),
                    })?;
                self.children_by_parent.insert(parent, children);
            } else {
                self.children_by_parent
                    .get_mut(&parent)
                    .ok_or(CatalogError::InvariantMissingChildIndex { parent })?
                    .try_reserve(1)
                    .map_err(|error| CatalogError::AllocationFailed {
                        resource: "source catalog child-step index",
                        requested: 1,
                        unit: CatalogAllocationUnit::Slots,
                        message: error.to_string(),
                    })?;
            }
        }
        Ok(())
    }

    fn checked_source_storage_bytes(
        &self,
        descriptor: &SourceDescriptor,
    ) -> Result<u64, CatalogError> {
        let mut bytes = checked_arc_allocation_bytes::<SourceLocator>()?;
        bytes = checked_byte_add(bytes, checked_arc_allocation_bytes::<Vec<u8>>()?)?;
        bytes = checked_byte_add(bytes, checked_arc_allocation_bytes::<SourceRecord>()?)?;
        bytes = checked_byte_add(
            bytes,
            checked_btree_entry_bytes::<SourceId, Arc<SourceRecord>>()?,
        )?;
        bytes = checked_byte_add(
            bytes,
            checked_hash_map_growth_bytes(&self.by_key, 1, "source catalog key index")?,
        )?;
        bytes = checked_byte_add(
            bytes,
            checked_hash_map_growth_bytes(&self.by_locator, 1, "source catalog locator index")?,
        )?;

        if matches!(&descriptor.placement, SourcePlacement::Root { .. }) {
            bytes = checked_byte_add(
                bytes,
                checked_hash_map_growth_bytes(
                    &self.physical_bindings,
                    1,
                    "source catalog physical-binding index",
                )?,
            )?;
        }
        if let SourcePlacement::Root { alias, .. } = &descriptor.placement {
            bytes = checked_byte_add(
                bytes,
                checked_hash_map_growth_bytes(
                    &self.root_aliases,
                    1,
                    "source catalog root-alias index",
                )?,
            )?;
            bytes = checked_byte_add(bytes, checked_arc_allocation_bytes::<SourceAlias>()?)?;
            bytes = checked_byte_add(bytes, checked_usize_to_u64(alias.retained_clone_bytes())?)?;
        }
        if let Some((parent, step)) = descriptor.child_step() {
            if !self.children_by_parent.contains_key(&parent) {
                bytes = checked_byte_add(
                    bytes,
                    checked_hash_map_growth_bytes(
                        &self.children_by_parent,
                        1,
                        "source catalog child index",
                    )?,
                )?;
                bytes = checked_byte_add(
                    bytes,
                    checked_empty_hash_map_bytes::<Arc<ContainmentStep>, SourceId>(1)?,
                )?;
            } else {
                let children = self
                    .children_by_parent
                    .get(&parent)
                    .ok_or(CatalogError::InvariantMissingChildIndex { parent })?;
                bytes = checked_byte_add(
                    bytes,
                    checked_hash_map_growth_bytes(children, 1, "source catalog child-step index")?,
                )?;
            }
            bytes = checked_byte_add(bytes, checked_arc_allocation_bytes::<ContainmentStep>()?)?;
            bytes = checked_byte_add(
                bytes,
                checked_usize_to_u64(step.member().retained_clone_bytes())?,
            )?;
        }
        Ok(bytes)
    }

    pub(crate) fn begin_transaction(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceCatalogTransaction, CatalogError> {
        self.validate()?;
        self.clone_transaction_candidate(budget)
    }

    /// Clones a catalog already owned by validated workspace/prepared state.
    ///
    /// The workspace-state transaction performs the one authoritative candidate validation when
    /// catalog and store are committed together.
    pub(in crate::workspace) fn begin_state_transaction(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceCatalogTransaction, CatalogError> {
        self.clone_transaction_candidate(budget)
    }

    fn clone_transaction_candidate(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceCatalogTransaction, CatalogError> {
        let retained_bytes = self.checked_transaction_clone_bytes()?;
        let entry_count =
            u64::try_from(self.by_id.len()).map_err(|_| CatalogError::AllocationSizeOverflow {
                resource: "source catalog transaction clone",
            })?;
        budget.check_entries(entry_count)?;
        budget.check_bytes(retained_bytes)?;

        let mut candidate = SourceCatalog::new(self.workspace);
        candidate
            .by_key
            .try_reserve(self.by_key.len())
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog transaction key index",
                requested: self.by_key.len(),
                unit: CatalogAllocationUnit::Slots,
                message: error.to_string(),
            })?;
        candidate
            .by_locator
            .try_reserve(self.by_locator.len())
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog transaction locator index",
                requested: self.by_locator.len(),
                unit: CatalogAllocationUnit::Slots,
                message: error.to_string(),
            })?;
        candidate
            .physical_bindings
            .try_reserve(self.physical_bindings.len())
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog transaction physical-binding index",
                requested: self.physical_bindings.len(),
                unit: CatalogAllocationUnit::Slots,
                message: error.to_string(),
            })?;
        candidate
            .root_aliases
            .try_reserve(self.root_aliases.len())
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog transaction root-alias index",
                requested: self.root_aliases.len(),
                unit: CatalogAllocationUnit::Slots,
                message: error.to_string(),
            })?;
        candidate
            .children_by_parent
            .try_reserve(self.children_by_parent.len())
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog transaction child index",
                requested: self.children_by_parent.len(),
                unit: CatalogAllocationUnit::Slots,
                message: error.to_string(),
            })?;
        for (parent, children) in &self.children_by_parent {
            let mut cloned = HashMap::new();
            cloned
                .try_reserve(children.len())
                .map_err(|error| CatalogError::AllocationFailed {
                    resource: "source catalog transaction child-step index",
                    requested: children.len(),
                    unit: CatalogAllocationUnit::Slots,
                    message: error.to_string(),
                })?;
            cloned.extend(
                children
                    .iter()
                    .map(|(step, source)| (Arc::clone(step), *source)),
            );
            candidate.children_by_parent.insert(*parent, cloned);
        }

        candidate.by_key.extend(
            self.by_key
                .iter()
                .map(|(key, source)| (Arc::clone(key), *source)),
        );
        candidate.by_locator.extend(
            self.by_locator
                .iter()
                .map(|(locator, source)| (Arc::clone(locator), *source)),
        );
        candidate.physical_bindings.extend(
            self.physical_bindings
                .iter()
                .map(|(origin, source)| (Arc::clone(origin), *source)),
        );
        candidate.root_aliases.extend(
            self.root_aliases
                .iter()
                .map(|(alias, source)| (Arc::clone(alias), *source)),
        );
        candidate.by_id.extend(
            self.by_id
                .iter()
                .map(|(source, record)| (*source, Arc::clone(record))),
        );
        budget.consume_entries(entry_count)?;
        budget.consume_bytes(retained_bytes)?;

        Ok(SourceCatalogTransaction {
            candidate,
            failed: false,
            #[cfg(test)]
            last_physical_domain_owner_resolutions: 0,
            #[cfg(test)]
            subtree_removal_index_visits: 0,
        })
    }

    fn checked_transaction_clone_bytes(&self) -> Result<u64, CatalogError> {
        let mut bytes = checked_empty_hash_map_bytes::<Arc<Vec<u8>>, SourceId>(self.by_key.len())?;
        bytes = checked_byte_add(
            bytes,
            checked_empty_hash_map_bytes::<Arc<SourceLocator>, SourceId>(self.by_locator.len())?,
        )?;
        bytes = checked_byte_add(
            bytes,
            checked_empty_hash_map_bytes::<Arc<PhysicalOrigin>, SourceId>(
                self.physical_bindings.len(),
            )?,
        )?;
        bytes = checked_byte_add(
            bytes,
            checked_empty_hash_map_bytes::<Arc<SourceAlias>, SourceId>(self.root_aliases.len())?,
        )?;
        bytes = checked_byte_add(
            bytes,
            checked_empty_hash_map_bytes::<SourceId, HashMap<Arc<ContainmentStep>, SourceId>>(
                self.children_by_parent.len(),
            )?,
        )?;
        for children in self.children_by_parent.values() {
            bytes = checked_byte_add(
                bytes,
                checked_empty_hash_map_bytes::<Arc<ContainmentStep>, SourceId>(children.len())?,
            )?;
        }
        let records = checked_usize_to_u64(self.by_id.len())?;
        let per_record = checked_btree_entry_bytes::<SourceId, Arc<SourceRecord>>()?;
        checked_byte_add(
            bytes,
            records
                .checked_mul(per_record)
                .ok_or(CatalogError::AllocationSizeOverflow {
                    resource: "source catalog transaction clone",
                })?,
        )
    }

    pub(crate) fn resolve(&self, source: SourceId) -> Result<&SourceDescriptor, CatalogError> {
        self.ensure_workspace(source)?;
        self.by_id
            .get(&source)
            .map(|record| &record.descriptor)
            .ok_or(CatalogError::UnknownSource(source))
    }

    pub(crate) fn source_locator(&self, source: SourceId) -> Result<&SourceLocator, CatalogError> {
        self.ensure_workspace(source)?;
        self.by_id
            .get(&source)
            .map(|record| record.source_locator.as_ref())
            .ok_or(CatalogError::UnknownSource(source))
    }

    #[must_use]
    pub(crate) fn find_root_by_alias(&self, alias: &SourceAlias) -> Option<SourceId> {
        self.root_aliases.get(alias).copied()
    }

    pub(crate) fn classify_locator(&self, locator: &SourceLocator) -> LocatorResolution {
        let Some(mut current) = self.find_root_by_alias(locator.root_alias()) else {
            return LocatorResolution::Unloaded;
        };
        for step in locator.members() {
            let expected = match current.kind() {
                SourceKind::Archive => ContainmentKind::Archive,
                SourceKind::AssetBundle => ContainmentKind::Bundle,
                SourceKind::WebFile => ContainmentKind::WebFile,
                SourceKind::Yaml | SourceKind::SerializedFile => ContainmentKind::Companion,
                SourceKind::StreamedResource => {
                    return LocatorResolution::Invalid;
                }
            };
            if step.container() != expected {
                return LocatorResolution::Invalid;
            }
            let Some(child) = self
                .children_by_parent
                .get(&current)
                .and_then(|children| children.get(step))
                .copied()
            else {
                return LocatorResolution::Missing;
            };
            current = child;
        }
        LocatorResolution::Resolved(current)
    }

    #[must_use]
    pub(crate) fn find_physical(&self, origin: &PhysicalOrigin) -> Option<SourceId> {
        self.physical_bindings.get(origin).copied()
    }

    pub(crate) fn physical_origin(
        &self,
        source: SourceId,
    ) -> Result<&PhysicalOrigin, CatalogError> {
        self.physical_origin_option(source)?
            .ok_or(CatalogError::UnboundPhysicalOrigin { source_id: source })
    }

    pub(crate) fn physical_origin_option(
        &self,
        source: SourceId,
    ) -> Result<Option<&PhysicalOrigin>, CatalogError> {
        self.ensure_workspace(source)?;
        Ok(self
            .by_id
            .get(&source)
            .ok_or(CatalogError::UnknownSource(source))?
            .physical_origin
            .as_deref())
    }

    pub(crate) fn address_for_object(
        &self,
        object: &ObjectId,
    ) -> Result<ObjectAddress, CatalogError> {
        self.ensure_workspace(object.source())?;
        let source_locator = self.source_locator(object.source())?.clone();
        match object.kind() {
            ObjectKind::Binary => ObjectAddress::binary_at(
                source_locator,
                object.binary_path_id().ok_or(CatalogError::UnknownObject {
                    source_id: object.source(),
                    kind: object.kind(),
                })?,
            ),
            ObjectKind::Yaml => {
                if let Some(file_id) = object.yaml_file_id() {
                    ObjectAddress::yaml(source_locator, file_id)
                } else if let Some(index) = object.yaml_document_ordinal() {
                    ObjectAddress::yaml_document(source_locator, index)
                } else {
                    return Err(CatalogError::UnknownObject {
                        source_id: object.source(),
                        kind: object.kind(),
                    });
                }
            }
        }
        .map_err(CatalogError::InvalidIdentity)
    }

    pub(crate) fn fingerprint(&self, source: SourceId) -> Result<SourceFingerprint, CatalogError> {
        self.ensure_workspace(source)?;
        self.by_id
            .get(&source)
            .map(|record| record.fingerprint)
            .ok_or(CatalogError::UnknownSource(source))
    }

    pub(crate) fn revision(&self) -> Result<WorkspaceRevision, CatalogError> {
        self.revision_with_fingerprint_lookup(|_| None)
    }

    pub(crate) fn revision_with_fingerprint_lookup(
        &self,
        mut fingerprint_override: impl FnMut(SourceId) -> Option<SourceFingerprint>,
    ) -> Result<WorkspaceRevision, CatalogError> {
        const PREFIX: &[u8] = b"unity-asset:source-catalog:v6\0";

        let mut logical_length = checked_len(PREFIX.len())?;
        logical_length = checked_add(logical_length, 16)?;
        for (source, record) in &self.by_id {
            logical_length = checked_add(logical_length, 16)?;
            logical_length = checked_add(
                logical_length,
                DigestV1Builder::framed_len(source.kind().tag().as_bytes())?,
            )?;
            logical_length = checked_add(
                logical_length,
                DigestV1Builder::framed_len(&record.canonical_key)?,
            )?;
            logical_length = checked_add(logical_length, DigestV1::BYTE_LEN as u64)?;
        }

        let mut digest = DigestV1Builder::new(logical_length);
        digest.update(PREFIX)?;
        digest.update(&self.workspace.get().to_le_bytes())?;
        for (source, record) in &self.by_id {
            digest.update(&source.local().to_le_bytes())?;
            digest.update_framed(source.kind().tag().as_bytes())?;
            digest.update_framed(&record.canonical_key)?;
            let fingerprint = fingerprint_override(*source).unwrap_or(record.fingerprint);
            if fingerprint.kind() != record.descriptor.kind {
                return Err(CatalogError::SourceKindMismatch {
                    expected: record.descriptor.kind,
                    actual: fingerprint.kind(),
                });
            }
            digest.update(fingerprint.digest().as_bytes())?;
        }
        Ok(WorkspaceRevision::new(digest.finalize()?))
    }

    /// Hashes the complete runtime installation without changing logical workspace identity.
    ///
    /// A logical revision deliberately excludes physical paths so relocating an otherwise
    /// identical project does not invalidate object addresses. Durable recovery needs the
    /// complementary proof: every source must still be attached to the same physical origin as
    /// the journal's base or committed state. The source identity plus optional canonical origin
    /// captures the complete physical-domain membership because contained sources inherit their
    /// domain owner's origin in the validated catalog.
    pub(crate) fn installation_digest(&self) -> Result<DigestV1, CatalogError> {
        const PREFIX: &[u8] = b"unity-asset:workspace-installation:v1\0";

        let mut logical_length = checked_len(PREFIX.len())?;
        logical_length = checked_add(logical_length, 16)?;
        for (source, record) in &self.by_id {
            logical_length = checked_add(logical_length, 16)?;
            logical_length = checked_add(
                logical_length,
                DigestV1Builder::framed_len(source.kind().tag().as_bytes())?,
            )?;
            logical_length = checked_add(logical_length, 1)?;
            if let Some(origin) = &record.physical_origin {
                logical_length = checked_add(
                    logical_length,
                    DigestV1Builder::framed_len(origin.path().as_os_str().as_encoded_bytes())?,
                )?;
            }
        }

        let mut digest = DigestV1Builder::new(logical_length);
        digest.update(PREFIX)?;
        digest.update(&self.workspace.get().to_le_bytes())?;
        for (source, record) in &self.by_id {
            digest.update(&source.local().to_le_bytes())?;
            digest.update_framed(source.kind().tag().as_bytes())?;
            match &record.physical_origin {
                Some(origin) => {
                    digest.update(&[1])?;
                    digest.update_framed(origin.path().as_os_str().as_encoded_bytes())?;
                }
                None => digest.update(&[0])?,
            }
        }
        digest.finalize().map_err(Into::into)
    }

    #[must_use]
    pub(crate) const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.by_id.len()
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = (SourceId, &SourceDescriptor)> {
        self.by_id
            .iter()
            .map(|(source, record)| (*source, &record.descriptor))
    }

    #[must_use]
    pub(crate) fn contains(&self, source: SourceId) -> bool {
        source.workspace() == self.workspace && self.by_id.contains_key(&source)
    }

    pub(crate) fn parent(&self, source: SourceId) -> Result<Option<SourceId>, CatalogError> {
        Ok(self.resolve(source)?.parent())
    }

    #[cfg(test)]
    pub(crate) fn child_by_step(
        &self,
        parent: SourceId,
        step: &ContainmentStep,
    ) -> Result<Option<SourceId>, CatalogError> {
        self.resolve(parent)?;
        Ok(self
            .children_by_parent
            .get(&parent)
            .and_then(|children| children.get(step))
            .copied())
    }

    pub(crate) fn child_by_member(
        &self,
        parent: SourceId,
        container: ContainmentKind,
        member: &SourceMemberId,
    ) -> Result<Option<SourceId>, CatalogError> {
        self.resolve(parent)?;
        Ok(self.children_by_parent.get(&parent).and_then(|children| {
            children.iter().find_map(|(step, source)| {
                (step.container() == container && step.member() == member).then_some(*source)
            })
        }))
    }

    #[cfg(test)]
    pub(crate) fn children(&self, source: SourceId) -> Result<Vec<SourceId>, CatalogError> {
        self.resolve(source)?;
        let mut children = self
            .children_by_parent
            .get(&source)
            .into_iter()
            .flat_map(HashMap::values)
            .copied()
            .collect::<Vec<_>>();
        children.sort_unstable();
        Ok(children)
    }

    pub(crate) fn validate(&self) -> Result<(), CatalogError> {
        for (source, record) in &self.by_id {
            if source.workspace() != self.workspace {
                return Err(CatalogError::InvariantWorkspaceMismatch {
                    source_id: *source,
                    expected: self.workspace,
                    actual: source.workspace(),
                });
            }
            if source.kind() != record.descriptor.kind || source.kind() != record.fingerprint.kind()
            {
                return Err(CatalogError::InvariantKindMismatch {
                    source_id: *source,
                    descriptor: record.descriptor.kind,
                    fingerprint: record.fingerprint.kind(),
                });
            }
            let expected_source = SourceId::new(
                self.workspace,
                record.descriptor.kind,
                deterministic_local_id(record.canonical_key.as_slice()),
            )
            .map_err(CatalogError::InvalidIdentity)?;
            if expected_source != *source {
                return Err(CatalogError::InvariantRecordMismatch {
                    source_id: *source,
                    field: "deterministic source id",
                });
            }
            if !canonical_source_key_matches(
                record.descriptor.kind,
                record.source_locator.as_ref(),
                record.canonical_key.as_slice(),
            )? {
                return Err(CatalogError::InvariantRecordMismatch {
                    source_id: *source,
                    field: "canonical source key",
                });
            }

            let Some((indexed_key, indexed_source)) =
                self.by_key.get_key_value(record.canonical_key.as_ref())
            else {
                return Err(CatalogError::InvariantMissingIndex {
                    source_id: *source,
                    index: "canonical key",
                });
            };
            if indexed_source != source || !Arc::ptr_eq(indexed_key, &record.canonical_key) {
                return Err(CatalogError::InvariantRecordMismatch {
                    source_id: *source,
                    field: "canonical-key index ownership",
                });
            }
            let Some((indexed_locator, indexed_source)) = self
                .by_locator
                .get_key_value(record.source_locator.as_ref())
            else {
                return Err(CatalogError::InvariantMissingIndex {
                    source_id: *source,
                    index: "source locator",
                });
            };
            if indexed_source != source || !Arc::ptr_eq(indexed_locator, &record.source_locator) {
                return Err(CatalogError::InvariantRecordMismatch {
                    source_id: *source,
                    field: "source-locator index ownership",
                });
            }

            match &record.descriptor.placement {
                SourcePlacement::Root {
                    alias,
                    physical_origin,
                } => {
                    let bound_origin = record.physical_origin.as_ref().ok_or(
                        CatalogError::InvariantRecordMismatch {
                            source_id: *source,
                            field: "root physical binding",
                        },
                    )?;
                    if !record.source_locator.members().is_empty()
                        || record.source_locator.root_alias() != alias
                        || bound_origin.as_ref() != physical_origin
                    {
                        return Err(CatalogError::InvariantRecordMismatch {
                            source_id: *source,
                            field: "root placement",
                        });
                    }
                    let Some((indexed_origin, indexed_source)) =
                        self.physical_bindings.get_key_value(bound_origin.as_ref())
                    else {
                        return Err(CatalogError::InvariantMissingIndex {
                            source_id: *source,
                            index: "physical origin",
                        });
                    };
                    if indexed_source != source || !Arc::ptr_eq(indexed_origin, bound_origin) {
                        return Err(CatalogError::InvariantRecordMismatch {
                            source_id: *source,
                            field: "physical-origin index ownership",
                        });
                    }
                    if self.root_aliases.get(alias) != Some(source) {
                        return Err(CatalogError::InvariantMissingIndex {
                            source_id: *source,
                            index: "root alias",
                        });
                    }
                }
                SourcePlacement::Member {
                    parent,
                    step,
                    location_kind,
                } => {
                    let parent_record =
                        self.by_id
                            .get(parent)
                            .ok_or(CatalogError::InvariantMissingParent {
                                source_id: *source,
                                parent: *parent,
                            })?;
                    if !valid_member_placement(
                        parent_record.descriptor.kind,
                        record.descriptor.kind,
                        step,
                        *location_kind,
                    ) || !same_optional_origin(
                        &record.physical_origin,
                        &parent_record.physical_origin,
                    ) || !locator_is_exact_child(
                        record.source_locator.as_ref(),
                        parent_record.source_locator.as_ref(),
                        step,
                    ) {
                        return Err(CatalogError::InvariantRecordMismatch {
                            source_id: *source,
                            field: "member placement",
                        });
                    }
                    if self
                        .children_by_parent
                        .get(parent)
                        .and_then(|children| children.get(step))
                        != Some(source)
                    {
                        return Err(CatalogError::InvariantMissingIndex {
                            source_id: *source,
                            index: "parent child step",
                        });
                    }
                }
                SourcePlacement::Companion { parent, step } => {
                    let parent_record =
                        self.by_id
                            .get(parent)
                            .ok_or(CatalogError::InvariantMissingParent {
                                source_id: *source,
                                parent: *parent,
                            })?;
                    if !supports_companion(parent_record.descriptor.kind)
                        || record.descriptor.kind != SourceKind::StreamedResource
                        || step.container() != ContainmentKind::Companion
                        || !locator_is_exact_child(
                            record.source_locator.as_ref(),
                            parent_record.source_locator.as_ref(),
                            step,
                        )
                    {
                        return Err(CatalogError::InvariantRecordMismatch {
                            source_id: *source,
                            field: "companion placement",
                        });
                    }
                    if let Some(bound_origin) = &record.physical_origin {
                        let Some((indexed_origin, indexed_source)) =
                            self.physical_bindings.get_key_value(bound_origin.as_ref())
                        else {
                            return Err(CatalogError::InvariantMissingIndex {
                                source_id: *source,
                                index: "physical origin",
                            });
                        };
                        if indexed_source != source || !Arc::ptr_eq(indexed_origin, bound_origin) {
                            return Err(CatalogError::InvariantRecordMismatch {
                                source_id: *source,
                                field: "physical-origin index ownership",
                            });
                        }
                    }
                    if self
                        .children_by_parent
                        .get(parent)
                        .and_then(|children| children.get(step))
                        != Some(source)
                    {
                        return Err(CatalogError::InvariantMissingIndex {
                            source_id: *source,
                            index: "parent companion step",
                        });
                    }
                }
            }
        }

        for (key, source) in &self.by_key {
            let record = self
                .by_id
                .get(source)
                .ok_or(CatalogError::InvariantMissingIndex {
                    source_id: *source,
                    index: "canonical-key target",
                })?;
            if !Arc::ptr_eq(key, &record.canonical_key) {
                return Err(CatalogError::InvariantRecordMismatch {
                    source_id: *source,
                    field: "canonical-key reverse index",
                });
            }
        }
        for (locator, source) in &self.by_locator {
            let record = self
                .by_id
                .get(source)
                .ok_or(CatalogError::InvariantMissingIndex {
                    source_id: *source,
                    index: "source-locator target",
                })?;
            if !Arc::ptr_eq(locator, &record.source_locator) {
                return Err(CatalogError::InvariantRecordMismatch {
                    source_id: *source,
                    field: "source-locator reverse index",
                });
            }
        }
        for (origin, source) in &self.physical_bindings {
            let record = self
                .by_id
                .get(source)
                .ok_or(CatalogError::InvariantMissingIndex {
                    source_id: *source,
                    index: "physical-origin target",
                })?;
            if !record.descriptor.has_independent_physical_origin()
                || record
                    .physical_origin
                    .as_ref()
                    .is_none_or(|bound_origin| !Arc::ptr_eq(origin, bound_origin))
            {
                return Err(CatalogError::InvariantRecordMismatch {
                    source_id: *source,
                    field: "physical-origin reverse index",
                });
            }
        }
        for (parent, children) in &self.children_by_parent {
            if children.is_empty() || !self.by_id.contains_key(parent) {
                return Err(CatalogError::InvariantUnexpectedChildIndex { parent: *parent });
            }
            for (step, child) in children {
                let record = self
                    .by_id
                    .get(child)
                    .ok_or(CatalogError::InvariantMissingIndex {
                        source_id: *child,
                        index: "child-step target",
                    })?;
                if record.descriptor.child_step() != Some((*parent, step.as_ref())) {
                    return Err(CatalogError::InvariantRecordMismatch {
                        source_id: *child,
                        field: "child-step reverse index",
                    });
                }
            }
        }
        if self.by_key.len() != self.by_id.len() || self.by_locator.len() != self.by_id.len() {
            return Err(CatalogError::InvariantIndexCardinality {
                records: self.by_id.len(),
                keys: self.by_key.len(),
                locators: self.by_locator.len(),
            });
        }
        let root_count = self
            .by_id
            .values()
            .filter(|record| record.descriptor.parent().is_none())
            .count();
        let physical_binding_count = self
            .by_id
            .values()
            .filter(|record| {
                record.descriptor.has_independent_physical_origin()
                    && record.physical_origin.is_some()
            })
            .count();
        let child_count = self.by_id.len().saturating_sub(root_count);
        let indexed_children = self
            .children_by_parent
            .values()
            .map(HashMap::len)
            .sum::<usize>();
        if self.root_aliases.len() != root_count
            || self.physical_bindings.len() != physical_binding_count
            || indexed_children != child_count
        {
            return Err(CatalogError::InvariantOwnershipIndexCardinality {
                roots: root_count,
                root_aliases: self.root_aliases.len(),
                expected_physical_bindings: physical_binding_count,
                physical_bindings: self.physical_bindings.len(),
                children: child_count,
                indexed_children,
            });
        }
        Ok(())
    }

    fn remove_subtree(
        &mut self,
        root: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<SubtreeRemoval, CatalogError> {
        self.resolve(root)?;
        let source_count = self.count_subtree_sources(root)?;
        let planned_bytes = vec_allocation_bytes::<SourceId>(source_count).map_err(|_| {
            CatalogError::AllocationSizeOverflow {
                resource: "source catalog subtree scratch",
            }
        })?;
        budget.check_bytes(planned_bytes)?;
        let mut removed = Vec::new();
        removed.try_reserve_exact(source_count).map_err(|error| {
            CatalogError::AllocationFailed {
                resource: "source catalog subtree scratch",
                requested: source_count,
                unit: CatalogAllocationUnit::Elements,
                message: error.to_string(),
            }
        })?;
        let retained_bytes =
            vec_allocation_bytes::<SourceId>(removed.capacity()).map_err(|_| {
                CatalogError::AllocationSizeOverflow {
                    resource: "source catalog subtree scratch",
                }
            })?;
        budget.check_bytes(retained_bytes)?;
        budget.consume_bytes(retained_bytes)?;

        self.collect_subtree_sources(root, &mut removed);
        debug_assert_eq!(removed.len(), source_count);
        removed.sort_unstable();
        for source in &removed {
            let record = self
                .by_id
                .remove(source)
                .ok_or(CatalogError::UnknownSource(*source))?;
            self.by_key.remove(&record.canonical_key);
            self.by_locator.remove(&record.source_locator);
            if record.descriptor.has_independent_physical_origin()
                && let Some(physical_origin) = &record.physical_origin
            {
                self.physical_bindings.remove(physical_origin);
            }
            match &record.descriptor.placement {
                SourcePlacement::Root { alias, .. } => {
                    self.root_aliases.remove(alias);
                }
                SourcePlacement::Member { parent, step, .. }
                | SourcePlacement::Companion { parent, step, .. } => {
                    let remove_parent_index =
                        self.children_by_parent
                            .get_mut(parent)
                            .is_some_and(|children| {
                                children.remove(step);
                                children.is_empty()
                            });
                    if remove_parent_index {
                        self.children_by_parent.remove(parent);
                    }
                }
            }
            self.children_by_parent.remove(source);
        }
        Ok(SubtreeRemoval {
            sources: removed,
            #[cfg(test)]
            index_visits: source_count.checked_mul(2).ok_or(
                CatalogError::AllocationSizeOverflow {
                    resource: "source catalog subtree visit count",
                },
            )?,
        })
    }

    fn count_subtree_sources(&self, source: SourceId) -> Result<usize, CatalogError> {
        let record = self
            .by_id
            .get(&source)
            .ok_or(CatalogError::UnknownSource(source))?;
        let parent_depth = record.source_locator.members().len();
        let mut count = 1_usize;
        if let Some(children) = self.children_by_parent.get(&source) {
            for child in children.values().copied() {
                let child_record =
                    self.by_id
                        .get(&child)
                        .ok_or(CatalogError::InvariantMissingIndex {
                            source_id: child,
                            index: "subtree child target",
                        })?;
                let expected_depth =
                    parent_depth
                        .checked_add(1)
                        .ok_or(CatalogError::AllocationSizeOverflow {
                            resource: "source catalog subtree depth",
                        })?;
                if child_record.descriptor.parent() != Some(source)
                    || child_record.source_locator.members().len() != expected_depth
                {
                    return Err(CatalogError::InvariantRecordMismatch {
                        source_id: child,
                        field: "subtree child index",
                    });
                }
                count = count
                    .checked_add(self.count_subtree_sources(child)?)
                    .ok_or(CatalogError::AllocationSizeOverflow {
                        resource: "source catalog subtree size",
                    })?;
            }
        }
        Ok(count)
    }

    fn collect_subtree_sources(&self, source: SourceId, output: &mut Vec<SourceId>) {
        output.push(source);
        if let Some(children) = self.children_by_parent.get(&source) {
            for child in children.values().copied() {
                self.collect_subtree_sources(child, output);
            }
        }
    }

    fn resolve_placement(
        &self,
        descriptor: &SourceDescriptor,
    ) -> Result<(SourceLocator, Option<Arc<PhysicalOrigin>>), CatalogError> {
        match &descriptor.placement {
            SourcePlacement::Root {
                alias,
                physical_origin,
            } => Ok((
                SourceLocator::path(alias.as_str()).map_err(CatalogError::InvalidIdentity)?,
                Some(Arc::new(physical_origin.clone())),
            )),
            SourcePlacement::Member { parent, step, .. } => {
                self.ensure_workspace(*parent)?;
                let parent_record = self
                    .by_id
                    .get(parent)
                    .ok_or(CatalogError::UnknownSource(*parent))?;
                let source_locator = parent_record
                    .source_locator
                    .as_ref()
                    .clone()
                    .child(step.container(), step.member().clone())
                    .map_err(CatalogError::InvalidIdentity)?;
                Ok((source_locator, parent_record.physical_origin.clone()))
            }
            SourcePlacement::Companion { parent, step } => {
                self.ensure_workspace(*parent)?;
                let parent_record = self
                    .by_id
                    .get(parent)
                    .ok_or(CatalogError::UnknownSource(*parent))?;
                if !supports_companion(parent_record.descriptor.kind) {
                    return Err(CatalogError::InvalidCompanionParentKind {
                        parent: *parent,
                        actual: parent_record.descriptor.kind,
                    });
                }
                let source_locator = parent_record
                    .source_locator
                    .as_ref()
                    .clone()
                    .child(step.container(), step.member().clone())
                    .map_err(CatalogError::InvalidIdentity)?;
                Ok((source_locator, None))
            }
        }
    }

    fn checked_placement_bytes(&self, descriptor: &SourceDescriptor) -> Result<u64, CatalogError> {
        match &descriptor.placement {
            SourcePlacement::Root {
                alias,
                physical_origin,
            } => {
                let mut bytes = checked_usize_to_u64(alias.retained_clone_bytes())?;
                bytes = checked_byte_add(
                    bytes,
                    checked_usize_to_u64(physical_origin.path().as_os_str().len())?,
                )?;
                checked_byte_add(bytes, checked_arc_allocation_bytes::<PhysicalOrigin>()?)
            }
            SourcePlacement::Member { parent, step, .. } => {
                self.ensure_workspace(*parent)?;
                let parent_record = self
                    .by_id
                    .get(parent)
                    .ok_or(CatalogError::UnknownSource(*parent))?;
                let locator_bytes = parent_record.source_locator.retained_clone_bytes().ok_or(
                    CatalogError::AllocationSizeOverflow {
                        resource: "source locator clone",
                    },
                )?;
                let mut bytes = checked_usize_to_u64(locator_bytes)?;
                bytes = checked_byte_add(
                    bytes,
                    checked_usize_to_u64(step.member().retained_clone_bytes())?,
                )?;
                checked_byte_add(
                    bytes,
                    checked_vec_growth_bytes::<ContainmentStep>(
                        parent_record.source_locator.members().len(),
                        1,
                    )?,
                )
            }
            SourcePlacement::Companion { parent, step } => {
                self.ensure_workspace(*parent)?;
                let parent_record = self
                    .by_id
                    .get(parent)
                    .ok_or(CatalogError::UnknownSource(*parent))?;
                if !supports_companion(parent_record.descriptor.kind) {
                    return Err(CatalogError::InvalidCompanionParentKind {
                        parent: *parent,
                        actual: parent_record.descriptor.kind,
                    });
                }
                let locator_bytes = parent_record.source_locator.retained_clone_bytes().ok_or(
                    CatalogError::AllocationSizeOverflow {
                        resource: "source locator clone",
                    },
                )?;
                let mut bytes = checked_usize_to_u64(locator_bytes)?;
                bytes = checked_byte_add(
                    bytes,
                    checked_usize_to_u64(step.member().retained_clone_bytes())?,
                )?;
                bytes = checked_byte_add(
                    bytes,
                    checked_vec_growth_bytes::<ContainmentStep>(
                        parent_record.source_locator.members().len(),
                        1,
                    )?,
                )?;
                Ok(bytes)
            }
        }
    }

    /// Freezes every physical domain affected by an ordered replacement set in two catalog scans.
    pub(crate) fn prepare_physical_domain_rewrite_batch<'change>(
        &self,
        changes: &'change [PhysicalDomainChange],
        budget: &mut AssetLoadBudget,
    ) -> Result<PhysicalDomainRewriteBatch<'change>, CatalogError> {
        ensure_physical_domain_changes_ordered(changes)?;

        let owner_entries = checked_usize_to_u64(changes.len())?;
        let owner_minimum =
            checked_vec_exact_bytes::<SourceId>(changes.len(), "physical domain rewrite owners")?;
        budget.check_entries(owner_entries)?;
        budget.check_bytes(owner_minimum)?;
        let mut owners = Vec::new();
        owners.try_reserve_exact(changes.len()).map_err(|error| {
            CatalogError::AllocationFailed {
                resource: "physical domain rewrite owners",
                requested: changes.len(),
                unit: CatalogAllocationUnit::Elements,
                message: error.to_string(),
            }
        })?;
        let owner_bytes = vec_allocation_bytes::<SourceId>(owners.capacity()).map_err(|_| {
            CatalogError::AllocationSizeOverflow {
                resource: "physical domain rewrite owners",
            }
        })?;
        budget.check_bytes(owner_bytes)?;

        #[cfg(test)]
        let mut owner_resolutions = 0_usize;
        for change in changes {
            owners.push(self.physical_domain_owner(change.source())?);
            #[cfg(test)]
            {
                owner_resolutions = owner_resolutions.checked_add(1).ok_or(
                    CatalogError::AllocationSizeOverflow {
                        resource: "physical domain owner resolution count",
                    },
                )?;
            }
        }
        owners.sort_unstable();
        owners.dedup();

        let mut observed_count = 0_usize;
        if !owners.is_empty() {
            for source in self.by_id.keys() {
                let owner = self.physical_domain_owner(*source)?;
                #[cfg(test)]
                {
                    owner_resolutions = owner_resolutions.checked_add(1).ok_or(
                        CatalogError::AllocationSizeOverflow {
                            resource: "physical domain owner resolution count",
                        },
                    )?;
                }
                if owners.binary_search(&owner).is_ok() {
                    observed_count = observed_count.checked_add(1).ok_or(
                        CatalogError::AllocationSizeOverflow {
                            resource: "physical domain rewrite observations",
                        },
                    )?;
                }
            }
        }

        let observed_entries = checked_usize_to_u64(observed_count)?;
        let total_entries = owner_entries.checked_add(observed_entries).ok_or(
            CatalogError::AllocationSizeOverflow {
                resource: "physical domain rewrite batch entries",
            },
        )?;
        let observed_minimum = checked_vec_exact_bytes::<ObservedPhysicalDomainSource>(
            observed_count,
            "physical domain rewrite observations",
        )?;
        let minimum_bytes = checked_byte_add(owner_bytes, observed_minimum)?;
        budget.check_entries(total_entries)?;
        budget.check_bytes(minimum_bytes)?;
        let mut observed = Vec::new();
        observed
            .try_reserve_exact(observed_count)
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "physical domain rewrite observations",
                requested: observed_count,
                unit: CatalogAllocationUnit::Elements,
                message: error.to_string(),
            })?;
        let observed_bytes = vec_allocation_bytes::<ObservedPhysicalDomainSource>(
            observed.capacity(),
        )
        .map_err(|_| CatalogError::AllocationSizeOverflow {
            resource: "physical domain rewrite observations",
        })?;
        let retained_bytes = checked_byte_add(owner_bytes, observed_bytes)?;
        budget.check_bytes(retained_bytes)?;

        if !owners.is_empty() {
            for (source, record) in &self.by_id {
                let owner = self.physical_domain_owner(*source)?;
                #[cfg(test)]
                {
                    owner_resolutions = owner_resolutions.checked_add(1).ok_or(
                        CatalogError::AllocationSizeOverflow {
                            resource: "physical domain owner resolution count",
                        },
                    )?;
                }
                if owners.binary_search(&owner).is_ok() {
                    observed.push(ObservedPhysicalDomainSource {
                        owner,
                        source: *source,
                        fingerprint: record.fingerprint,
                    });
                }
            }
        }
        debug_assert_eq!(observed.len(), observed_count);

        budget.consume_entries(total_entries)?;
        budget.consume_bytes(retained_bytes)?;
        Ok(PhysicalDomainRewriteBatch {
            workspace: self.workspace,
            owners,
            observed,
            changes,
            #[cfg(test)]
            owner_resolutions,
        })
    }

    fn apply_physical_domain_rewrite_batch(
        &mut self,
        batch: PhysicalDomainRewriteBatch<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<usize, CatalogError> {
        let owner_resolutions = self.validate_physical_domain_rewrite_batch(&batch)?;

        let mut prepared_count = 0_usize;
        let mut record_bytes = 0_u64;
        for change in batch.changes {
            let record = self
                .by_id
                .get(&change.source())
                .ok_or(CatalogError::UnknownSource(change.source()))?;
            if change.replacement().kind() != record.descriptor.kind {
                return Err(CatalogError::SourceKindMismatch {
                    expected: record.descriptor.kind,
                    actual: change.replacement().kind(),
                });
            }
            if batch
                .observed
                .binary_search_by_key(&change.source(), |observed| observed.source)
                .is_err()
            {
                return Err(CatalogError::PhysicalDomainChangeOutsideDomain {
                    source_id: change.source(),
                    physical_owner: self.physical_domain_owner(change.source())?,
                });
            }
            if record.fingerprint != change.replacement() {
                prepared_count =
                    prepared_count
                        .checked_add(1)
                        .ok_or(CatalogError::AllocationSizeOverflow {
                            resource: "physical domain prepared changes",
                        })?;
                record_bytes = checked_byte_add(
                    record_bytes,
                    checked_record_clone_bytes(&record.descriptor)?,
                )?;
            }
        }

        let prepared_entries = checked_usize_to_u64(prepared_count)?;
        let prepared_minimum = checked_vec_exact_bytes::<PreparedPhysicalDomainChange>(
            prepared_count,
            "physical domain prepared changes",
        )?;
        let minimum_bytes = checked_byte_add(prepared_minimum, record_bytes)?;
        budget.check_entries(prepared_entries)?;
        budget.check_bytes(minimum_bytes)?;
        let mut prepared = Vec::new();
        prepared
            .try_reserve_exact(prepared_count)
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "physical domain prepared changes",
                requested: prepared_count,
                unit: CatalogAllocationUnit::Elements,
                message: error.to_string(),
            })?;
        let prepared_bytes = vec_allocation_bytes::<PreparedPhysicalDomainChange>(
            prepared.capacity(),
        )
        .map_err(|_| CatalogError::AllocationSizeOverflow {
            resource: "physical domain prepared changes",
        })?;
        let planned_bytes = checked_byte_add(prepared_bytes, record_bytes)?;
        budget.check_bytes(planned_bytes)?;

        for change in batch.changes {
            let record = self
                .by_id
                .get(&change.source())
                .expect("batch validation proved every changed source");
            if record.fingerprint == change.replacement() {
                continue;
            }
            prepared.push(PreparedPhysicalDomainChange {
                source: change.source(),
                record: Arc::new(SourceRecord {
                    descriptor: record.descriptor.clone(),
                    fingerprint: change.replacement(),
                    source_locator: Arc::clone(&record.source_locator),
                    physical_origin: record.physical_origin.clone(),
                    canonical_key: Arc::clone(&record.canonical_key),
                }),
            });
        }
        debug_assert_eq!(prepared.len(), prepared_count);

        budget.consume_entries(prepared_entries)?;
        budget.consume_bytes(planned_bytes)?;
        for change in prepared {
            let record = self
                .by_id
                .get_mut(&change.source)
                .expect("batch validation proved every changed source");
            *record = change.record;
        }
        Ok(owner_resolutions)
    }

    fn validate_physical_domain_rewrite_batch(
        &self,
        batch: &PhysicalDomainRewriteBatch<'_>,
    ) -> Result<usize, CatalogError> {
        if batch.workspace != self.workspace {
            return Err(CatalogError::WorkspaceMismatch {
                expected: self.workspace,
                actual: batch.workspace,
            });
        }

        let mut owner_resolutions = 0_usize;
        let mut observed = batch.observed.iter();
        let mut next_observed = observed.next();
        for (source, record) in &self.by_id {
            let owner = self.physical_domain_owner(*source)?;
            owner_resolutions =
                owner_resolutions
                    .checked_add(1)
                    .ok_or(CatalogError::AllocationSizeOverflow {
                        resource: "physical domain owner resolution count",
                    })?;
            if let Some(expected) = next_observed
                && expected.source < *source
            {
                return Err(CatalogError::PhysicalDomainObservationUnexpected {
                    owner: expected.owner,
                    source_id: expected.source,
                });
            }

            match next_observed {
                Some(expected) if expected.source == *source => {
                    if expected.owner != owner {
                        return Err(CatalogError::PhysicalDomainMembershipMismatch(
                            PhysicalDomainMembershipDrift::new(*source, expected.owner, owner),
                        ));
                    }
                    if expected.fingerprint != record.fingerprint {
                        return Err(CatalogError::PhysicalDomainFingerprintMismatch {
                            source_id: *source,
                            expected: expected.fingerprint,
                            actual: record.fingerprint,
                        });
                    }
                    next_observed = observed.next();
                }
                _ if batch.owners.binary_search(&owner).is_ok() => {
                    return Err(CatalogError::PhysicalDomainObservationMissing {
                        owner,
                        source_id: *source,
                    });
                }
                _ => {}
            }
        }
        if let Some(expected) = next_observed {
            return Err(CatalogError::PhysicalDomainObservationUnexpected {
                owner: expected.owner,
                source_id: expected.source,
            });
        }
        Ok(owner_resolutions)
    }

    pub(crate) fn physical_domain_owner(&self, source: SourceId) -> Result<SourceId, CatalogError> {
        self.ensure_workspace(source)?;
        let mut current = source;
        for _ in 0..=self.by_id.len() {
            let record = self
                .by_id
                .get(&current)
                .ok_or(CatalogError::UnknownSource(current))?;
            match &record.descriptor.placement {
                SourcePlacement::Member { parent, .. } => current = *parent,
                SourcePlacement::Root { .. } | SourcePlacement::Companion { .. } => {
                    return Ok(current);
                }
            }
        }
        Err(CatalogError::InvariantOwnershipCycle { source_id: source })
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
        if let Some(existing) = self.physical_bindings.get(origin)
            && *existing != source
        {
            return Err(CatalogError::PhysicalOriginConflict {
                existing: *existing,
                incoming: source,
            });
        }
        Ok(())
    }
}

/// Fallible candidate catalog. Once an operation fails, the candidate cannot be committed.
pub(crate) struct SourceCatalogTransaction {
    candidate: SourceCatalog,
    failed: bool,
    #[cfg(test)]
    last_physical_domain_owner_resolutions: usize,
    #[cfg(test)]
    subtree_removal_index_visits: usize,
}

impl SourceCatalogTransaction {
    /// Borrows the fallible candidate without converting it into authoritative state.
    ///
    /// Publication preflight uses this view to derive durable installation evidence before the
    /// candidate is moved into the workspace-state transaction for joint catalog/store validation.
    pub(in crate::workspace) fn candidate(&self) -> &SourceCatalog {
        &self.candidate
    }

    pub(crate) fn root_admission_decision(
        &mut self,
        alias: &SourceAlias,
        origin: &PhysicalOrigin,
        fingerprint: SourceFingerprint,
    ) -> Result<RootAdmissionDecision, CatalogError> {
        self.ensure_active()?;
        match self
            .candidate
            .root_admission_decision(alias, origin, fingerprint)
        {
            Ok(decision) => Ok(decision),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    pub(crate) fn is_root(&mut self, source: SourceId) -> Result<bool, CatalogError> {
        self.ensure_active()?;
        match self.candidate.resolve(source) {
            Ok(descriptor) => Ok(descriptor.parent().is_none()),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    pub(crate) fn register(
        &mut self,
        descriptor: SourceDescriptor,
        fingerprint: SourceFingerprint,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, CatalogError> {
        self.ensure_active()?;
        match self
            .candidate
            .register_impl(descriptor, fingerprint, Some(budget))
        {
            Ok(source) => Ok(source),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    pub(crate) fn bind_companion_origin(
        &mut self,
        source: SourceId,
        origin: PhysicalOrigin,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), CatalogError> {
        self.ensure_active()?;
        match self
            .candidate
            .bind_companion_origin_impl(source, origin, budget)
        {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn register_companion(
        &mut self,
        parent: SourceId,
        member: SourceMemberId,
        fingerprint: SourceFingerprint,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, CatalogError> {
        self.ensure_active()?;
        let descriptor = match SourceDescriptor::companion(parent, member) {
            Ok(descriptor) => descriptor,
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        };
        self.register(descriptor, fingerprint, budget)
    }

    /// Applies a complete multi-domain CAS batch without rescanning once per owner.
    pub(crate) fn rewrite_physical_domains(
        &mut self,
        batch: PhysicalDomainRewriteBatch<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), CatalogError> {
        self.ensure_active()?;
        match self
            .candidate
            .apply_physical_domain_rewrite_batch(batch, budget)
        {
            Ok(owner_resolutions) => {
                #[cfg(test)]
                {
                    self.last_physical_domain_owner_resolutions = owner_resolutions;
                }
                let _ = owner_resolutions;
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    /// Builds and applies a complete physical-domain rewrite against the
    /// transaction candidate, including sources registered earlier in the
    /// same recovery delta.
    pub(crate) fn rewrite_physical_domains_from_changes(
        &mut self,
        changes: &[PhysicalDomainChange],
        budget: &mut AssetLoadBudget,
    ) -> Result<(), CatalogError> {
        self.ensure_active()?;
        let batch = match self
            .candidate
            .prepare_physical_domain_rewrite_batch(changes, budget)
        {
            Ok(batch) => batch,
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        };
        self.rewrite_physical_domains(batch, budget)
    }

    pub(crate) fn remove_subtree(
        &mut self,
        root: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SourceId>, CatalogError> {
        self.ensure_active()?;
        match self.candidate.remove_subtree(root, budget) {
            Ok(removed) => {
                #[cfg(test)]
                {
                    self.subtree_removal_index_visits += removed.index_visits;
                }
                Ok(removed.sources)
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn replace_subtree(
        &mut self,
        root: SourceId,
        replacements: impl IntoIterator<Item = (SourceDescriptor, SourceFingerprint)>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SourceId>, CatalogError> {
        self.ensure_active()?;
        match self.candidate.remove_subtree(root, budget) {
            Ok(removed) => {
                self.subtree_removal_index_visits += removed.index_visits;
            }
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        }
        let mut inserted = Vec::new();
        for (descriptor, fingerprint) in replacements {
            match self
                .candidate
                .register_impl(descriptor, fingerprint, Some(&mut *budget))
            {
                Ok(source) => inserted.push(source),
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            }
        }
        Ok(inserted)
    }

    pub(crate) fn commit(
        self,
        _budget: &mut AssetLoadBudget,
    ) -> Result<SourceCatalog, CatalogError> {
        if self.failed {
            return Err(CatalogError::TransactionAborted);
        }
        self.candidate.validate()?;
        Ok(self.candidate)
    }

    /// Transfers the candidate to the workspace-state transaction for joint validation.
    ///
    /// Callers that only own a catalog candidate must use [`Self::commit`]. The workspace-state
    /// transaction validates catalog and source-store invariants together exactly once.
    pub(in crate::workspace) fn into_state_candidate(self) -> Result<SourceCatalog, CatalogError> {
        if self.failed {
            return Err(CatalogError::TransactionAborted);
        }
        Ok(self.candidate)
    }

    fn ensure_active(&self) -> Result<(), CatalogError> {
        if self.failed {
            Err(CatalogError::TransactionAborted)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogAllocationUnit {
    Bytes,
    Elements,
    Slots,
}

impl std::fmt::Display for CatalogAllocationUnit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Bytes => "bytes",
            Self::Elements => "elements",
            Self::Slots => "slots",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhysicalDomainMembershipDrift {
    source_id: SourceId,
    expected_owner_kind: SourceKind,
    expected_owner_local: u128,
    actual_owner_kind: SourceKind,
    actual_owner_local: u128,
}

impl PhysicalDomainMembershipDrift {
    fn new(source_id: SourceId, expected_owner: SourceId, actual_owner: SourceId) -> Self {
        debug_assert_eq!(source_id.workspace(), expected_owner.workspace());
        debug_assert_eq!(source_id.workspace(), actual_owner.workspace());
        Self {
            source_id,
            expected_owner_kind: expected_owner.kind(),
            expected_owner_local: expected_owner.local(),
            actual_owner_kind: actual_owner.kind(),
            actual_owner_local: actual_owner.local(),
        }
    }

    fn expected_owner(&self) -> SourceId {
        SourceId::new(
            self.source_id.workspace(),
            self.expected_owner_kind,
            self.expected_owner_local,
        )
        .expect("physical domain owners retain nonzero source identities")
    }

    fn actual_owner(&self) -> SourceId {
        SourceId::new(
            self.source_id.workspace(),
            self.actual_owner_kind,
            self.actual_owner_local,
        )
        .expect("physical domain owners retain nonzero source identities")
    }
}

impl std::fmt::Display for PhysicalDomainMembershipDrift {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "physical domain source {:?} moved from {:?} to {:?}",
            self.source_id,
            self.expected_owner(),
            self.actual_owner()
        )
    }
}

impl std::error::Error for PhysicalDomainMembershipDrift {}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum CatalogError {
    #[error("invalid source identity: {0}")]
    InvalidIdentity(ContractError),
    #[error(transparent)]
    InvalidPhysicalOrigin(#[from] PhysicalOriginError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("failed to reserve {requested} {unit} for {resource}: {message}")]
    AllocationFailed {
        resource: &'static str,
        requested: usize,
        unit: CatalogAllocationUnit,
        message: String,
    },
    #[error("allocation size overflow for {resource}")]
    AllocationSizeOverflow { resource: &'static str },
    #[error("source catalog transaction was aborted by an earlier failure")]
    TransactionAborted,
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
    #[error("companion parent {parent:?} must be a serialized file or YAML source, got {actual:?}")]
    InvalidCompanionParentKind {
        parent: SourceId,
        actual: SourceKind,
    },
    #[error(
        "deterministic source identity collision for {source_id:?} with existing kind {existing_kind:?}"
    )]
    IdentityCollision {
        source_id: SourceId,
        existing_kind: SourceKind,
    },
    #[error("source locator collision between {existing:?} and {incoming:?}")]
    LocatorCollision {
        existing: SourceId,
        incoming: SourceId,
    },
    #[error("source {source_id:?} fingerprint changed during registration")]
    FingerprintConflict {
        source_id: SourceId,
        expected: SourceFingerprint,
        actual: SourceFingerprint,
    },
    #[error("physical origin maps to both {existing:?} and {incoming:?}")]
    PhysicalOriginConflict {
        existing: SourceId,
        incoming: SourceId,
    },
    #[error("source {source_id:?} is already bound to a different physical origin")]
    PhysicalOriginChanged { source_id: SourceId },
    #[error("source {source_id:?} is not a companion and cannot receive a companion origin")]
    PhysicalOriginRequiresCompanion { source_id: SourceId },
    #[error(
        "physical domain {collection} sources must be strictly ordered: {previous:?} precedes {current:?}"
    )]
    PhysicalDomainSourcesNotStrictlyOrdered {
        collection: &'static str,
        previous: SourceId,
        current: SourceId,
    },
    #[error("physical domain {owner:?} observation is missing source {source_id:?}")]
    PhysicalDomainObservationMissing {
        owner: SourceId,
        source_id: SourceId,
    },
    #[error("physical domain {owner:?} observation contains unexpected source {source_id:?}")]
    PhysicalDomainObservationUnexpected {
        owner: SourceId,
        source_id: SourceId,
    },
    #[error(
        "physical domain source {source_id:?} fingerprint mismatch: expected {expected:?}, got {actual:?}"
    )]
    PhysicalDomainFingerprintMismatch {
        source_id: SourceId,
        expected: SourceFingerprint,
        actual: SourceFingerprint,
    },
    #[error(transparent)]
    PhysicalDomainMembershipMismatch(PhysicalDomainMembershipDrift),
    #[error("source {source_id:?} belongs to a different physical domain {physical_owner:?}")]
    PhysicalDomainChangeOutsideDomain {
        source_id: SourceId,
        physical_owner: SourceId,
    },
    #[error("source {source_id:?} does not yet have a physical origin")]
    UnboundPhysicalOrigin { source_id: SourceId },
    #[error("failed to verify physical binding {path:?}: {message}")]
    VerifiedPhysicalBindingIo {
        path: PathBuf,
        kind: io::ErrorKind,
        message: String,
    },
    #[error("physical binding changed while it was being verified: {path:?}")]
    VerifiedPhysicalBindingChanged { path: PathBuf },
    #[error("verified file fingerprint mismatch: expected {expected:?}, got {actual:?}")]
    VerifiedFingerprintMismatch {
        expected: SourceFingerprint,
        actual: SourceFingerprint,
    },
    #[error("source belongs to workspace {actual}, not {expected}")]
    WorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("unknown source id: {0:?}")]
    UnknownSource(SourceId),
    #[error("{kind:?} object source {source_id:?} does not resolve in this catalog")]
    UnknownObject {
        source_id: SourceId,
        kind: ObjectKind,
    },
    #[error("source catalog record {source_id:?} belongs to workspace {actual}, not {expected}")]
    InvariantWorkspaceMismatch {
        source_id: SourceId,
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error(
        "source catalog record {source_id:?} has descriptor kind {descriptor:?} and fingerprint kind {fingerprint:?}"
    )]
    InvariantKindMismatch {
        source_id: SourceId,
        descriptor: SourceKind,
        fingerprint: SourceKind,
    },
    #[error("source catalog record {source_id:?} has missing parent {parent:?}")]
    InvariantMissingParent {
        source_id: SourceId,
        parent: SourceId,
    },
    #[error("source catalog record {source_id:?} is missing its {index} index")]
    InvariantMissingIndex {
        source_id: SourceId,
        index: &'static str,
    },
    #[error("source catalog record {source_id:?} has inconsistent {field}")]
    InvariantRecordMismatch {
        source_id: SourceId,
        field: &'static str,
    },
    #[error(
        "source catalog index cardinality mismatch: records={records}, keys={keys}, locators={locators}"
    )]
    InvariantIndexCardinality {
        records: usize,
        keys: usize,
        locators: usize,
    },
    #[error(
        "source catalog ownership index mismatch: roots={roots}, aliases={root_aliases}, expected_physical={expected_physical_bindings}, physical={physical_bindings}, children={children}, indexed_children={indexed_children}"
    )]
    InvariantOwnershipIndexCardinality {
        roots: usize,
        root_aliases: usize,
        expected_physical_bindings: usize,
        physical_bindings: usize,
        children: usize,
        indexed_children: usize,
    },
    #[error("source catalog ownership cycle contains {source_id:?}")]
    InvariantOwnershipCycle { source_id: SourceId },
    #[error("source catalog has no child index for parent {parent:?}")]
    InvariantMissingChildIndex { parent: SourceId },
    #[error("source catalog has an invalid or empty child index for parent {parent:?}")]
    InvariantUnexpectedChildIndex { parent: SourceId },
}

impl CatalogError {
    fn verified_binding_io(path: &Path, error: io::Error) -> Self {
        Self::VerifiedPhysicalBindingIo {
            path: path.to_path_buf(),
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

fn checked_record_clone_bytes(descriptor: &SourceDescriptor) -> Result<u64, CatalogError> {
    checked_byte_add(
        checked_arc_allocation_bytes::<SourceRecord>()?,
        checked_descriptor_clone_bytes(descriptor)?,
    )
}

fn checked_descriptor_clone_bytes(descriptor: &SourceDescriptor) -> Result<u64, CatalogError> {
    match &descriptor.placement {
        SourcePlacement::Root {
            alias,
            physical_origin,
        } => checked_byte_add(
            checked_usize_to_u64(alias.retained_clone_bytes())?,
            checked_usize_to_u64(physical_origin.path().as_os_str().len())?,
        ),
        SourcePlacement::Member { step, .. } => {
            checked_usize_to_u64(step.member().retained_clone_bytes())
        }
        SourcePlacement::Companion { step, .. } => {
            checked_usize_to_u64(step.member().retained_clone_bytes())
        }
    }
}

fn checked_vec_exact_bytes<T>(count: usize, resource: &'static str) -> Result<u64, CatalogError> {
    count
        .checked_mul(size_of::<T>())
        .ok_or(CatalogError::AllocationSizeOverflow { resource })
        .and_then(checked_usize_to_u64)
}

fn ensure_physical_domain_changes_ordered(
    changes: &[PhysicalDomainChange],
) -> Result<(), CatalogError> {
    for pair in changes.windows(2) {
        if pair[0].source() >= pair[1].source() {
            return Err(CatalogError::PhysicalDomainSourcesNotStrictlyOrdered {
                collection: "changes",
                previous: pair[0].source(),
                current: pair[1].source(),
            });
        }
    }
    Ok(())
}

fn ensure_regular_member_kind(kind: SourceKind) -> Result<(), CatalogError> {
    if kind == SourceKind::StreamedResource {
        Err(CatalogError::StreamedResourceRequiresSidecar)
    } else {
        Ok(())
    }
}

const fn supports_companion(kind: SourceKind) -> bool {
    matches!(kind, SourceKind::SerializedFile | SourceKind::Yaml)
}

fn valid_member_placement(
    parent_kind: SourceKind,
    child_kind: SourceKind,
    step: &ContainmentStep,
    location_kind: SourceLocationKind,
) -> bool {
    let expected_container = match parent_kind {
        SourceKind::Archive => ContainmentKind::Archive,
        SourceKind::WebFile => ContainmentKind::WebFile,
        SourceKind::AssetBundle => ContainmentKind::Bundle,
        SourceKind::Yaml | SourceKind::SerializedFile | SourceKind::StreamedResource => {
            return false;
        }
    };
    if step.container() != expected_container {
        return false;
    }
    match location_kind {
        SourceLocationKind::Root => false,
        SourceLocationKind::ArchiveMember => {
            expected_container == ContainmentKind::Archive
                && child_kind != SourceKind::StreamedResource
        }
        SourceLocationKind::WebFileMember => {
            expected_container == ContainmentKind::WebFile
                && child_kind != SourceKind::StreamedResource
        }
        SourceLocationKind::BundleMember => {
            expected_container == ContainmentKind::Bundle
                && child_kind != SourceKind::StreamedResource
        }
        SourceLocationKind::Sidecar => child_kind == SourceKind::StreamedResource,
        SourceLocationKind::Companion => false,
    }
}

fn locator_is_exact_child(
    child: &SourceLocator,
    parent: &SourceLocator,
    step: &ContainmentStep,
) -> bool {
    child.root_alias() == parent.root_alias()
        && child.members().len() == parent.members().len().saturating_add(1)
        && child
            .members()
            .split_last()
            .is_some_and(|(last, prefix)| last == step && prefix == parent.members())
}

fn same_optional_origin(
    left: &Option<Arc<PhysicalOrigin>>,
    right: &Option<Arc<PhysicalOrigin>>,
) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

fn canonical_source_key(
    kind: SourceKind,
    locator: &SourceLocator,
) -> Result<Vec<u8>, CatalogError> {
    let capacity = canonical_source_key_len(kind, locator)?;
    let mut key = Vec::new();
    key.try_reserve_exact(capacity)
        .map_err(|error| CatalogError::AllocationFailed {
            resource: "canonical source key",
            requested: capacity,
            unit: CatalogAllocationUnit::Bytes,
            message: error.to_string(),
        })?;
    key.extend_from_slice(b"unity-asset:source:v2\0");
    append_bytes(&mut key, kind.tag().as_bytes());
    append_bytes(&mut key, locator.root_alias().as_str().as_bytes());
    key.extend_from_slice(&(locator.members().len() as u64).to_le_bytes());
    for step in locator.members() {
        append_bytes(&mut key, step.container().tag().as_bytes());
        append_bytes(&mut key, step.member().name().as_bytes());
        key.extend_from_slice(&step.member().same_name_occurrence().to_le_bytes());
    }
    debug_assert_eq!(key.len(), capacity);
    Ok(key)
}

fn canonical_source_key_matches(
    kind: SourceKind,
    locator: &SourceLocator,
    key: &[u8],
) -> Result<bool, CatalogError> {
    if canonical_source_key_len(kind, locator)? != key.len() {
        return Ok(false);
    }
    let mut offset = 0_usize;
    if !matches_bytes(key, &mut offset, b"unity-asset:source:v2\0")
        || !matches_framed_bytes(key, &mut offset, kind.tag().as_bytes())
        || !matches_framed_bytes(key, &mut offset, locator.root_alias().as_str().as_bytes())
        || !matches_bytes(
            key,
            &mut offset,
            &(locator.members().len() as u64).to_le_bytes(),
        )
    {
        return Ok(false);
    }
    for step in locator.members() {
        if !matches_framed_bytes(key, &mut offset, step.container().tag().as_bytes())
            || !matches_framed_bytes(key, &mut offset, step.member().name().as_bytes())
            || !matches_bytes(
                key,
                &mut offset,
                &step.member().same_name_occurrence().to_le_bytes(),
            )
        {
            return Ok(false);
        }
    }
    Ok(offset == key.len())
}

fn matches_framed_bytes(key: &[u8], offset: &mut usize, expected: &[u8]) -> bool {
    matches_bytes(key, offset, &(expected.len() as u64).to_le_bytes())
        && matches_bytes(key, offset, expected)
}

fn matches_bytes(key: &[u8], offset: &mut usize, expected: &[u8]) -> bool {
    let Some(end) = offset.checked_add(expected.len()) else {
        return false;
    };
    if key.get(*offset..end) != Some(expected) {
        return false;
    }
    *offset = end;
    true
}

fn canonical_source_key_len(
    kind: SourceKind,
    locator: &SourceLocator,
) -> Result<usize, CatalogError> {
    canonical_source_key_len_parts(kind, locator.root_alias(), locator.members(), None)
}

fn canonical_source_key_len_parts(
    kind: SourceKind,
    root_alias: &SourceAlias,
    members: &[ContainmentStep],
    trailing_step: Option<&ContainmentStep>,
) -> Result<usize, CatalogError> {
    let mut length = b"unity-asset:source:v2\0".len();
    length = checked_usize_add(length, size_of::<u64>())?;
    length = checked_usize_add(length, kind.tag().len())?;
    length = checked_usize_add(length, size_of::<u64>())?;
    length = checked_usize_add(length, root_alias.as_str().len())?;
    length = checked_usize_add(length, size_of::<u64>())?;
    for step in members.iter().chain(trailing_step) {
        length = checked_usize_add(length, size_of::<u64>())?;
        length = checked_usize_add(length, step.container().tag().len())?;
        length = checked_usize_add(length, size_of::<u64>())?;
        length = checked_usize_add(length, step.member().name().len())?;
        length = checked_usize_add(length, size_of::<u32>())?;
    }
    Ok(length)
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

fn checked_usize_add(left: usize, right: usize) -> Result<usize, CatalogError> {
    left.checked_add(right)
        .ok_or(CatalogError::AllocationSizeOverflow {
            resource: "canonical source key",
        })
}

fn checked_arc_allocation_bytes<T>() -> Result<u64, CatalogError> {
    arc_value_allocation_bytes::<T>().map_err(|_| CatalogError::AllocationSizeOverflow {
        resource: "source catalog Arc allocation",
    })
}

fn checked_btree_entry_bytes<K, V>() -> Result<u64, CatalogError> {
    // A newly allocated BTree node is sparse. Charge a complete conservative node for every
    // logical entry so the first insertion cannot hide the unused key/value slots.
    const MAX_NODE_SLOTS: usize = 32;
    const NODE_METADATA_WORDS: usize = 8;
    let slot_bytes = size_of::<(K, V)>()
        .checked_add(size_of::<usize>().saturating_mul(2))
        .ok_or(CatalogError::AllocationSizeOverflow {
            resource: "source catalog ordered index",
        })?;
    let bytes = slot_bytes
        .checked_mul(MAX_NODE_SLOTS)
        .and_then(|value| value.checked_add(size_of::<usize>().saturating_mul(NODE_METADATA_WORDS)))
        .ok_or(CatalogError::AllocationSizeOverflow {
            resource: "source catalog ordered index",
        })?;
    checked_usize_to_u64(bytes)
}

fn checked_hash_map_growth_bytes<K, V>(
    map: &HashMap<K, V>,
    additional: usize,
    resource: &'static str,
) -> Result<u64, CatalogError> {
    let required = map
        .len()
        .checked_add(additional)
        .ok_or(CatalogError::AllocationSizeOverflow { resource })?;
    if required <= map.capacity() {
        return Ok(0);
    }
    checked_hash_table_bytes::<K, V>(required, resource)
}

fn checked_empty_hash_map_bytes<K, V>(required: usize) -> Result<u64, CatalogError> {
    checked_hash_table_bytes::<K, V>(required, "source catalog transaction hash index")
}

fn checked_hash_table_bytes<K, V>(
    required: usize,
    resource: &'static str,
) -> Result<u64, CatalogError> {
    if required == 0 {
        return Ok(0);
    }
    // HashMap capacity and control-byte layout are intentionally unspecified. Four times the
    // next power of two covers load-factor slack, growth slack, and control-byte alignment.
    let slots = required
        .checked_next_power_of_two()
        .and_then(|value| value.checked_mul(4))
        .ok_or(CatalogError::AllocationSizeOverflow { resource })?;
    let slot_bytes = size_of::<(K, V)>()
        .checked_add(size_of::<usize>())
        .ok_or(CatalogError::AllocationSizeOverflow { resource })?;
    let bytes = slots
        .checked_mul(slot_bytes)
        .ok_or(CatalogError::AllocationSizeOverflow { resource })?;
    checked_usize_to_u64(bytes)
}

fn checked_vec_growth_bytes<T>(current_len: usize, additional: usize) -> Result<u64, CatalogError> {
    if additional == 0 {
        return Ok(0);
    }
    let required =
        current_len
            .checked_add(additional)
            .ok_or(CatalogError::AllocationSizeOverflow {
                resource: "source locator growth",
            })?;
    let slots = required
        .checked_next_power_of_two()
        .and_then(|value| value.max(4).checked_mul(2))
        .ok_or(CatalogError::AllocationSizeOverflow {
            resource: "source locator growth",
        })?;
    let bytes = slots
        .checked_mul(size_of::<T>())
        .ok_or(CatalogError::AllocationSizeOverflow {
            resource: "source locator growth",
        })?;
    checked_usize_to_u64(bytes)
}

fn checked_usize_to_u64(value: usize) -> Result<u64, CatalogError> {
    u64::try_from(value).map_err(|_| CatalogError::AllocationSizeOverflow {
        resource: "source catalog allocation",
    })
}

fn checked_byte_add(left: u64, right: u64) -> Result<u64, CatalogError> {
    left.checked_add(right)
        .ok_or(CatalogError::AllocationSizeOverflow {
            resource: "source catalog allocation",
        })
}

fn deterministic_local_id(key: &[u8]) -> u128 {
    let digest = DigestV1::hash_bytes(key);
    let mut prefix = [0_u8; 16];
    prefix.copy_from_slice(&digest.as_bytes()[..16]);
    u128::from_le_bytes(prefix).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn physical_origin(name: &str, contents: &[u8]) -> PhysicalOrigin {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(name.replace('/', "-"));
        fs::write(&path, contents).unwrap();
        PhysicalOrigin::from_existing_path(path).unwrap()
    }

    fn root_descriptor(kind: SourceKind, alias: &str, contents: &[u8]) -> SourceDescriptor {
        SourceDescriptor::root(
            kind,
            SourceAlias::new(alias).unwrap(),
            physical_origin(alias, contents),
        )
    }

    fn fingerprint(kind: SourceKind, contents: &[u8]) -> SourceFingerprint {
        SourceFingerprint::from_bytes(kind, contents)
    }

    fn budget_with(bytes: u64, entries: u64) -> AssetLoadBudget {
        let limits = unity_asset_core::AssetLoadLimits {
            max_bytes: bytes.max(1),
            max_entries: entries.max(1),
            ..unity_asset_core::AssetLoadLimits::default()
        };
        AssetLoadBudget::new(limits).unwrap()
    }

    fn overallocated_path(capacity: usize) -> PathBuf {
        let mut path = PathBuf::with_capacity(capacity);
        path.push("canonical");
        path
    }

    fn physical_domain_fixture() -> (SourceCatalog, SourceId, SourceId, SourceId, SourceId) {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let root = catalog
            .register(
                root_descriptor(SourceKind::Archive, "game.apk", b"archive"),
                fingerprint(SourceKind::Archive, b"archive"),
            )
            .unwrap();
        let webfile = catalog
            .register(
                SourceDescriptor::archive_member(
                    root,
                    SourceKind::WebFile,
                    SourceMemberId::new("data.web").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::WebFile, b"webfile"),
            )
            .unwrap();
        let serialized_file = catalog
            .register(
                SourceDescriptor::webfile_member(
                    webfile,
                    SourceKind::SerializedFile,
                    SourceMemberId::new("main.assets").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::SerializedFile, b"asset"),
            )
            .unwrap();
        let companion = catalog
            .register(
                SourceDescriptor::companion(
                    serialized_file,
                    SourceMemberId::new("main.resS").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::StreamedResource, b"companion"),
            )
            .unwrap();
        (catalog, root, webfile, serialized_file, companion)
    }

    fn replace_candidate_fingerprint(
        transaction: &mut SourceCatalogTransaction,
        source: SourceId,
        fingerprint: SourceFingerprint,
    ) {
        let record = transaction.candidate.by_id.get(&source).unwrap();
        let replacement = Arc::new(SourceRecord {
            descriptor: record.descriptor.clone(),
            fingerprint,
            source_locator: Arc::clone(&record.source_locator),
            physical_origin: record.physical_origin.clone(),
            canonical_key: Arc::clone(&record.canonical_key),
        });
        *transaction.candidate.by_id.get_mut(&source).unwrap() = replacement;
    }

    #[test]
    fn multi_domain_batch_is_linear_exact_budgeted_and_atomic() {
        let (mut catalog, root, webfile, serialized_file, companion) = physical_domain_fixture();
        let other_root = catalog
            .register(
                root_descriptor(SourceKind::Archive, "other.apk", b"other archive"),
                fingerprint(SourceKind::Archive, b"other archive"),
            )
            .unwrap();
        let other_member = catalog
            .register(
                SourceDescriptor::archive_member(
                    other_root,
                    SourceKind::SerializedFile,
                    SourceMemberId::new("other.assets").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::SerializedFile, b"other asset"),
            )
            .unwrap();
        let unchanged_serialized = catalog.fingerprint(serialized_file).unwrap();
        let mut changes = vec![
            PhysicalDomainChange::new(
                webfile,
                fingerprint(SourceKind::WebFile, b"changed webfile"),
            ),
            PhysicalDomainChange::new(
                companion,
                fingerprint(SourceKind::StreamedResource, b"changed companion"),
            ),
            PhysicalDomainChange::new(
                other_member,
                fingerprint(SourceKind::SerializedFile, b"changed other asset"),
            ),
        ];
        changes.sort_unstable_by_key(PhysicalDomainChange::source);

        let source_count = catalog.by_id.len();
        let mut measured_build = AssetLoadBudget::default();
        let measured_batch = catalog
            .prepare_physical_domain_rewrite_batch(&changes, &mut measured_build)
            .unwrap();
        assert_eq!(measured_batch.owners.len(), 3);
        assert_eq!(measured_batch.observed.len(), source_count);
        assert_eq!(
            measured_batch.owner_resolutions,
            changes.len() + 2 * source_count
        );
        let build_usage = measured_build.usage();
        drop(measured_batch);

        let mut exact_build = budget_with(build_usage.bytes, build_usage.entries);
        let exact_batch = catalog
            .prepare_physical_domain_rewrite_batch(&changes, &mut exact_build)
            .unwrap();
        assert_eq!(exact_build.usage(), build_usage);
        drop(exact_batch);

        let mut short_build = budget_with(build_usage.bytes - 1, build_usage.entries);
        assert!(matches!(
            catalog.prepare_physical_domain_rewrite_batch(&changes, &mut short_build),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(short_build.usage().bytes, 0);
        assert_eq!(short_build.usage().entries, 0);

        let batch = catalog
            .prepare_physical_domain_rewrite_batch(&changes, &mut AssetLoadBudget::default())
            .unwrap();
        let mut transaction = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        let mut measured_apply = AssetLoadBudget::default();
        transaction
            .rewrite_physical_domains(batch, &mut measured_apply)
            .unwrap();
        assert_eq!(
            transaction.last_physical_domain_owner_resolutions,
            source_count
        );
        let apply_usage = measured_apply.usage();
        let candidate = transaction.commit(&mut measured_apply).unwrap();
        for change in &changes {
            assert_eq!(
                candidate.fingerprint(change.source()).unwrap(),
                change.replacement()
            );
        }
        assert_eq!(
            candidate.fingerprint(serialized_file).unwrap(),
            unchanged_serialized
        );
        assert_eq!(candidate.physical_domain_owner(root).unwrap(), root);

        let exact_batch = catalog
            .prepare_physical_domain_rewrite_batch(&changes, &mut AssetLoadBudget::default())
            .unwrap();
        let mut exact_transaction = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        let mut exact_apply = budget_with(apply_usage.bytes, apply_usage.entries);
        exact_transaction
            .rewrite_physical_domains(exact_batch, &mut exact_apply)
            .unwrap();
        assert_eq!(exact_apply.usage(), apply_usage);

        let rejected_batch = catalog
            .prepare_physical_domain_rewrite_batch(&changes, &mut AssetLoadBudget::default())
            .unwrap();
        let mut rejected = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        let before = rejected.candidate.revision().unwrap();
        let mut one_short = budget_with(apply_usage.bytes - 1, apply_usage.entries);
        assert!(matches!(
            rejected.rewrite_physical_domains(rejected_batch, &mut one_short),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(rejected.candidate.revision().unwrap(), before);
        assert_eq!(one_short.usage().bytes, 0);
        assert_eq!(one_short.usage().entries, 0);
        assert!(matches!(
            rejected.commit(&mut one_short),
            Err(CatalogError::TransactionAborted)
        ));
    }

    #[test]
    fn physical_domain_preparation_failure_sticky_aborts_transaction() {
        let (catalog, _, webfile, _, _) = physical_domain_fixture();
        let changes = [PhysicalDomainChange::new(
            webfile,
            fingerprint(SourceKind::WebFile, b"changed webfile"),
        )];
        let mut measured = AssetLoadBudget::default();
        let prepared = catalog
            .prepare_physical_domain_rewrite_batch(&changes, &mut measured)
            .unwrap();
        drop(prepared);
        let usage = measured.usage();

        let mut transaction = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        let mut one_short = budget_with(usage.bytes - 1, usage.entries);
        assert!(matches!(
            transaction.rewrite_physical_domains_from_changes(&changes, &mut one_short),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert!(matches!(
            transaction.commit(&mut one_short),
            Err(CatalogError::TransactionAborted)
        ));
    }

    #[test]
    fn physical_domain_batch_ignores_unaffected_domain_drift() {
        let (mut catalog, _, webfile, _, _) = physical_domain_fixture();
        let other_root = catalog
            .register(
                root_descriptor(SourceKind::Archive, "other.apk", b"other archive"),
                fingerprint(SourceKind::Archive, b"other archive"),
            )
            .unwrap();
        let other_member = catalog
            .register(
                SourceDescriptor::archive_member(
                    other_root,
                    SourceKind::SerializedFile,
                    SourceMemberId::new("other.assets").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::SerializedFile, b"other asset"),
            )
            .unwrap();
        let replacement = fingerprint(SourceKind::WebFile, b"prepared webfile");
        let drift = fingerprint(SourceKind::SerializedFile, b"unrelated drift");
        let changes = [PhysicalDomainChange::new(webfile, replacement)];
        let batch = catalog
            .prepare_physical_domain_rewrite_batch(&changes, &mut AssetLoadBudget::default())
            .unwrap();
        let mut transaction = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        replace_candidate_fingerprint(&mut transaction, other_member, drift);

        transaction
            .rewrite_physical_domains(batch, &mut AssetLoadBudget::default())
            .unwrap();
        let candidate = transaction.commit(&mut AssetLoadBudget::default()).unwrap();
        assert_eq!(candidate.fingerprint(webfile).unwrap(), replacement);
        assert_eq!(candidate.fingerprint(other_member).unwrap(), drift);
    }

    #[test]
    fn physical_domain_batch_validates_later_domain_before_applying_earlier_changes() {
        let (mut catalog, _, webfile, _, _) = physical_domain_fixture();
        let other_root = catalog
            .register(
                root_descriptor(SourceKind::Archive, "other.apk", b"other archive"),
                fingerprint(SourceKind::Archive, b"other archive"),
            )
            .unwrap();
        let other_member = catalog
            .register(
                SourceDescriptor::archive_member(
                    other_root,
                    SourceKind::SerializedFile,
                    SourceMemberId::new("other.assets").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::SerializedFile, b"other asset"),
            )
            .unwrap();
        let mut changes = vec![
            PhysicalDomainChange::new(
                webfile,
                fingerprint(SourceKind::WebFile, b"prepared webfile"),
            ),
            PhysicalDomainChange::new(
                other_member,
                fingerprint(SourceKind::SerializedFile, b"prepared other asset"),
            ),
        ];
        changes.sort_unstable_by_key(PhysicalDomainChange::source);
        let earlier = changes[0].source();
        let later = changes[1].source();
        let earlier_original = catalog.fingerprint(earlier).unwrap();
        let batch = catalog
            .prepare_physical_domain_rewrite_batch(&changes, &mut AssetLoadBudget::default())
            .unwrap();
        let mut transaction = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        replace_candidate_fingerprint(
            &mut transaction,
            later,
            fingerprint(later.kind(), b"concurrent later drift"),
        );
        let mut budget = AssetLoadBudget::default();

        assert!(matches!(
            transaction.rewrite_physical_domains(batch, &mut budget),
            Err(CatalogError::PhysicalDomainFingerprintMismatch { source_id, .. })
                if source_id == later
        ));
        assert_eq!(
            transaction.candidate.fingerprint(earlier).unwrap(),
            earlier_original
        );
        assert_eq!(budget.usage().bytes, 0);
        assert_eq!(budget.usage().entries, 0);
        assert!(matches!(
            transaction.commit(&mut budget),
            Err(CatalogError::TransactionAborted)
        ));
    }

    #[test]
    fn multi_domain_batch_rejects_fingerprint_and_membership_cas_drift() {
        let (mut catalog, root, webfile, serialized_file, _) = physical_domain_fixture();
        let other_root = catalog
            .register(
                root_descriptor(SourceKind::Archive, "other.apk", b"other archive"),
                fingerprint(SourceKind::Archive, b"other archive"),
            )
            .unwrap();
        let changes = [PhysicalDomainChange::new(
            webfile,
            fingerprint(SourceKind::WebFile, b"prepared webfile"),
        )];

        let stale_batch = catalog
            .prepare_physical_domain_rewrite_batch(&changes, &mut AssetLoadBudget::default())
            .unwrap();
        let mut stale = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        let record = stale.candidate.by_id.get(&webfile).unwrap();
        let replacement = Arc::new(SourceRecord {
            descriptor: record.descriptor.clone(),
            fingerprint: fingerprint(SourceKind::WebFile, b"concurrent webfile"),
            source_locator: Arc::clone(&record.source_locator),
            physical_origin: record.physical_origin.clone(),
            canonical_key: Arc::clone(&record.canonical_key),
        });
        *stale.candidate.by_id.get_mut(&webfile).unwrap() = replacement;
        let mut stale_budget = AssetLoadBudget::default();
        assert!(matches!(
            stale.rewrite_physical_domains(stale_batch, &mut stale_budget),
            Err(CatalogError::PhysicalDomainFingerprintMismatch {
                source_id,
                ..
            }) if source_id == webfile
        ));
        assert_eq!(stale_budget.usage().bytes, 0);
        assert_eq!(stale_budget.usage().entries, 0);
        assert!(matches!(
            stale.commit(&mut stale_budget),
            Err(CatalogError::TransactionAborted)
        ));

        let moved_batch = catalog
            .prepare_physical_domain_rewrite_batch(&changes, &mut AssetLoadBudget::default())
            .unwrap();
        let mut moved = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        let record = moved.candidate.by_id.get(&webfile).unwrap();
        let SourcePlacement::Member {
            step,
            location_kind,
            ..
        } = &record.descriptor.placement
        else {
            panic!("fixture webfile must be an inherited member");
        };
        let replacement = Arc::new(SourceRecord {
            descriptor: SourceDescriptor {
                kind: record.descriptor.kind,
                placement: SourcePlacement::Member {
                    parent: other_root,
                    step: step.clone(),
                    location_kind: *location_kind,
                },
            },
            fingerprint: record.fingerprint,
            source_locator: Arc::clone(&record.source_locator),
            physical_origin: record.physical_origin.clone(),
            canonical_key: Arc::clone(&record.canonical_key),
        });
        *moved.candidate.by_id.get_mut(&webfile).unwrap() = replacement;
        let mut moved_budget = AssetLoadBudget::default();
        let error = moved
            .rewrite_physical_domains(moved_batch, &mut moved_budget)
            .unwrap_err();
        assert!(
            matches!(
                &error,
                CatalogError::PhysicalDomainMembershipMismatch(details)
                    if (details.source_id == webfile || details.source_id == serialized_file)
                        && details.expected_owner() == root
                        && details.actual_owner() == other_root
            ),
            "unexpected membership error: {error:?}"
        );
        assert_eq!(moved_budget.usage().bytes, 0);
        assert_eq!(moved_budget.usage().entries, 0);
    }

    #[test]
    fn multi_domain_batch_rejects_order_missing_and_unexpected_sources() {
        let (catalog, root, webfile, serialized_file, _) = physical_domain_fixture();
        let mut reversed = vec![
            PhysicalDomainChange::new(
                webfile,
                fingerprint(SourceKind::WebFile, b"changed webfile"),
            ),
            PhysicalDomainChange::new(
                serialized_file,
                fingerprint(SourceKind::SerializedFile, b"changed asset"),
            ),
        ];
        reversed.sort_unstable_by_key(PhysicalDomainChange::source);
        reversed.reverse();
        let mut order_budget = AssetLoadBudget::default();
        assert!(matches!(
            catalog.prepare_physical_domain_rewrite_batch(&reversed, &mut order_budget),
            Err(CatalogError::PhysicalDomainSourcesNotStrictlyOrdered {
                collection: "changes",
                ..
            })
        ));
        assert_eq!(order_budget.usage().bytes, 0);
        assert_eq!(order_budget.usage().entries, 0);

        let changes = [PhysicalDomainChange::new(
            webfile,
            fingerprint(SourceKind::WebFile, b"changed webfile"),
        )];
        let missing_batch = catalog
            .prepare_physical_domain_rewrite_batch(&changes, &mut AssetLoadBudget::default())
            .unwrap();
        let mut missing = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        let late_source = missing
            .register(
                SourceDescriptor::archive_member(
                    root,
                    SourceKind::Yaml,
                    SourceMemberId::new("late.asset").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::Yaml, b"late"),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let mut missing_budget = AssetLoadBudget::default();
        assert!(matches!(
            missing.rewrite_physical_domains(missing_batch, &mut missing_budget),
            Err(CatalogError::PhysicalDomainObservationMissing {
                owner,
                source_id,
            }) if owner == root && source_id == late_source
        ));
        assert_eq!(missing_budget.usage().bytes, 0);
        assert_eq!(missing_budget.usage().entries, 0);

        let unexpected_batch = catalog
            .prepare_physical_domain_rewrite_batch(&changes, &mut AssetLoadBudget::default())
            .unwrap();
        let mut unexpected = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        unexpected.candidate.by_id.remove(&serialized_file);
        let mut unexpected_budget = AssetLoadBudget::default();
        assert!(matches!(
            unexpected.rewrite_physical_domains(unexpected_batch, &mut unexpected_budget),
            Err(CatalogError::PhysicalDomainObservationUnexpected {
                owner,
                source_id,
            }) if owner == root && source_id == serialized_file
        ));
        assert_eq!(unexpected_budget.usage().bytes, 0);
        assert_eq!(unexpected_budget.usage().entries, 0);
    }

    #[test]
    fn revision_fingerprint_lookup_matches_an_equivalent_domain_rewrite() {
        let (catalog, _root, webfile, serialized_file, _) = physical_domain_fixture();
        assert_eq!(
            catalog.revision().unwrap(),
            catalog.revision_with_fingerprint_lookup(|_| None).unwrap()
        );
        assert_eq!(
            catalog.revision().unwrap(),
            catalog
                .revision_with_fingerprint_lookup(|source| {
                    Some(catalog.fingerprint(source).unwrap())
                })
                .unwrap()
        );
        assert!(matches!(
            catalog.revision_with_fingerprint_lookup(|source| {
                (source == webfile).then(|| fingerprint(SourceKind::Yaml, b"wrong kind"))
            }),
            Err(CatalogError::SourceKindMismatch {
                expected: SourceKind::WebFile,
                actual: SourceKind::Yaml,
            })
        ));

        let mut changed = vec![
            PhysicalDomainChange::new(
                webfile,
                fingerprint(SourceKind::WebFile, b"changed webfile"),
            ),
            PhysicalDomainChange::new(
                serialized_file,
                fingerprint(SourceKind::SerializedFile, b"changed assets"),
            ),
        ];
        changed.sort_unstable_by_key(PhysicalDomainChange::source);
        let predicted = catalog
            .revision_with_fingerprint_lookup(|source| {
                changed
                    .iter()
                    .find(|change| change.source() == source)
                    .map(PhysicalDomainChange::replacement)
            })
            .unwrap();
        let rewrite = catalog
            .prepare_physical_domain_rewrite_batch(&changed, &mut AssetLoadBudget::default())
            .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        transaction
            .rewrite_physical_domains(rewrite, &mut AssetLoadBudget::default())
            .unwrap();
        let candidate = transaction.commit(&mut AssetLoadBudget::default()).unwrap();
        assert_eq!(candidate.revision().unwrap(), predicted);
    }

    #[test]
    fn revision_excludes_root_physical_binding_from_logical_identity() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.assets");
        let second_path = directory.path().join("second.assets");
        fs::write(&first_path, b"same bytes").unwrap();
        fs::write(&second_path, b"same bytes").unwrap();
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let source_fingerprint = fingerprint(SourceKind::SerializedFile, b"same bytes");

        let mut first = SourceCatalog::new(workspace);
        let first_source = first
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("logical.assets").unwrap(),
                    PhysicalOrigin::from_existing_path(&first_path).unwrap(),
                ),
                source_fingerprint,
            )
            .unwrap();
        let mut second = SourceCatalog::new(workspace);
        let second_source = second
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("logical.assets").unwrap(),
                    PhysicalOrigin::from_existing_path(&second_path).unwrap(),
                ),
                source_fingerprint,
            )
            .unwrap();

        assert_eq!(first_source, second_source);
        assert_eq!(
            first.source_locator(first_source).unwrap(),
            second.source_locator(second_source).unwrap()
        );
        assert_eq!(first.revision().unwrap(), second.revision().unwrap());
    }

    #[test]
    fn canonical_path_observation_budgets_actual_pathbuf_capacity_atomically() {
        const REQUESTED_CAPACITY: usize = 4_096;
        let one_short_path = overallocated_path(REQUESTED_CAPACITY);
        let planned = checked_usize_to_u64(one_short_path.capacity()).unwrap();
        assert!(one_short_path.capacity() >= REQUESTED_CAPACITY);
        assert!(one_short_path.capacity() > one_short_path.as_os_str().len());

        let one_short = budget_with(planned - 1, 1);
        assert!(matches!(
            BudgetedCanonicalPath::from_path(one_short_path, 0, &one_short),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit == planned - 1 && requested == planned
        ));
        assert_eq!(one_short.usage().bytes, 0);
        assert_eq!(one_short.usage().entries, 0);

        let exact_path = overallocated_path(REQUESTED_CAPACITY);
        let exact_bytes = checked_usize_to_u64(exact_path.capacity()).unwrap();
        let exact = budget_with(exact_bytes, 1);
        let observed = BudgetedCanonicalPath::from_path(exact_path, 0, &exact).unwrap();
        assert_eq!(observed.planned_bytes(), exact_bytes);
        assert_eq!(exact.usage().bytes, 0);
        assert_eq!(exact.usage().entries, 0);
    }

    #[test]
    fn physical_origin_materialization_is_exact_budgeted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.assets");
        fs::write(&path, b"asset").unwrap();
        let canonical = BudgetedCanonicalPath::resolve(&path, &AssetLoadBudget::default()).unwrap();
        let planned = canonical.planned_bytes();

        let mut one_short = budget_with(planned - 1, 1);
        assert!(matches!(
            PhysicalOrigin::from_existing_path_budgeted(&path, &mut one_short),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit == planned - 1 && requested == planned
        ));
        assert_eq!(one_short.usage().bytes, 0);

        let mut exact = budget_with(planned, 1);
        let origin = PhysicalOrigin::from_existing_path_budgeted(&path, &mut exact).unwrap();
        assert_eq!(origin.path(), canonical.path());
        assert_eq!(exact.usage().bytes, planned);
        assert_eq!(exact.usage().entries, 0);
    }

    #[test]
    fn verified_binding_construction_is_exact_budgeted_and_rejects_wrong_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.assets");
        let contents = b"verified asset bytes";
        fs::write(&path, contents).unwrap();
        let expected = fingerprint(SourceKind::SerializedFile, contents);
        let canonical = BudgetedCanonicalPath::resolve(&path, &AssetLoadBudget::default()).unwrap();
        assert!(
            canonical.planned_bytes()
                >= checked_usize_to_u64(canonical.path().as_os_str().len()).unwrap()
        );
        let planned = checked_byte_add(contents.len() as u64, canonical.planned_bytes()).unwrap();

        let mut wrong_kind_budget = AssetLoadBudget::default();
        assert!(matches!(
            VerifiedPhysicalBinding::verify_existing(
                SourceKind::SerializedFile,
                &path,
                fingerprint(SourceKind::Yaml, contents),
                &mut wrong_kind_budget,
            ),
            Err(CatalogError::SourceKindMismatch {
                expected: SourceKind::SerializedFile,
                actual: SourceKind::Yaml,
            })
        ));
        assert_eq!(wrong_kind_budget.usage().bytes, 0);

        let mut one_short = budget_with(planned - 1, 1);
        assert!(matches!(
            VerifiedPhysicalBinding::verify_existing(
                SourceKind::SerializedFile,
                &path,
                expected,
                &mut one_short,
            ),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(one_short.usage().bytes, 0);

        let mut exact = budget_with(planned, 1);
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &path,
            expected,
            &mut exact,
        )
        .unwrap();
        assert_eq!(binding.kind, SourceKind::SerializedFile);
        assert_eq!(binding.fingerprint, expected);
        let retained_path_bytes =
            checked_usize_to_u64(binding.physical_origin.0.capacity()).unwrap();
        let requested_path_bytes = checked_usize_to_u64(path.as_os_str().len()).unwrap();
        assert_eq!(
            planned,
            checked_byte_add(
                contents.len() as u64,
                retained_path_bytes.max(requested_path_bytes),
            )
            .unwrap()
        );
        assert_eq!(exact.usage().bytes, planned);

        let mut mismatch_budget = AssetLoadBudget::default();
        assert!(matches!(
            VerifiedPhysicalBinding::verify_existing(
                SourceKind::SerializedFile,
                &path,
                fingerprint(SourceKind::SerializedFile, b"different bytes"),
                &mut mismatch_budget,
            ),
            Err(CatalogError::VerifiedFingerprintMismatch { actual, .. })
                if actual == expected
        ));
        assert_eq!(mismatch_budget.usage().bytes, 0);

        fs::write(&path, b"changed after verification").unwrap();
        assert!(matches!(
            binding.revalidate_current_contents(&mut AssetLoadBudget::default()),
            Err(CatalogError::VerifiedPhysicalBindingChanged { .. })
        ));
    }

    #[test]
    fn verified_directory_binding_canonical_path_is_exact_budgeted() {
        let directory = tempfile::tempdir().unwrap();
        let canonical =
            BudgetedCanonicalPath::resolve(directory.path(), &AssetLoadBudget::default()).unwrap();
        let construction_bytes = canonical.planned_bytes();

        let mut construction_one_short = budget_with(construction_bytes - 1, 1);
        assert!(matches!(
            VerifiedPhysicalDirectoryBinding::verify_existing(
                directory.path(),
                &mut construction_one_short,
            ),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(construction_one_short.usage().bytes, 0);
        assert_eq!(construction_one_short.usage().entries, 0);

        let mut construction_exact = budget_with(construction_bytes, 1);
        let binding = VerifiedPhysicalDirectoryBinding::verify_existing(
            directory.path(),
            &mut construction_exact,
        )
        .unwrap();
        assert_eq!(binding.path(), canonical.path());
        let retained_path_bytes = checked_usize_to_u64(binding.path.capacity()).unwrap();
        assert!(construction_bytes >= retained_path_bytes);
        assert_eq!(construction_exact.usage().bytes, construction_bytes);
        assert_eq!(construction_exact.usage().entries, 1);

        let revalidation =
            BudgetedCanonicalPath::resolve(binding.path(), &AssetLoadBudget::default()).unwrap();
        let revalidation_bytes = revalidation.planned_bytes();
        let mut revalidation_one_short = budget_with(revalidation_bytes - 1, 1);
        assert!(matches!(
            binding.revalidate_current_entry(&mut revalidation_one_short),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(revalidation_one_short.usage().bytes, 0);
        assert_eq!(revalidation_one_short.usage().entries, 0);

        let mut revalidation_exact = budget_with(revalidation_bytes, 1);
        binding
            .revalidate_current_entry(&mut revalidation_exact)
            .unwrap();
        assert_eq!(revalidation_exact.usage().bytes, revalidation_bytes);
        assert_eq!(revalidation_exact.usage().entries, 1);
    }

    #[test]
    fn verified_directory_binding_ignores_child_entry_metadata_changes() {
        let directory = tempfile::tempdir().unwrap();
        let binding = VerifiedPhysicalDirectoryBinding::verify_existing(
            directory.path(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

        let recovery = directory.path().join(".unity-asset-recovery");
        fs::create_dir(&recovery).unwrap();
        fs::write(recovery.join("journal"), b"durable evidence").unwrap();

        binding
            .revalidate_current_entry(&mut AssetLoadBudget::default())
            .unwrap();
    }

    #[test]
    fn verified_binding_revalidation_is_exact_budgeted_even_for_noop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.assets");
        let contents = b"stable asset bytes";
        fs::write(&path, contents).unwrap();
        let source_fingerprint = fingerprint(SourceKind::SerializedFile, contents);

        let mut verification_budget = AssetLoadBudget::default();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &path,
            source_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let planned = binding.revalidation_bytes();
        assert_eq!(planned, contents.len() as u64);
        let mut one_short = budget_with(planned - 1, 1);
        assert!(matches!(
            binding.revalidate_current_contents(&mut one_short),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(one_short.usage().bytes, 0);
        assert_eq!(one_short.usage().entries, 0);

        let mut exact = budget_with(planned, 1);
        binding.revalidate_current_contents(&mut exact).unwrap();
        assert_eq!(exact.usage().bytes, planned);
        assert_eq!(exact.usage().entries, 0);
    }

    #[test]
    fn verified_binding_revalidation_rejects_same_length_in_place_rewrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.assets");
        let original_contents = b"AAAA";
        let changed_contents = b"BBBB";
        fs::write(&path, original_contents).unwrap();
        let original_modified = fs::metadata(&path).unwrap().modified().unwrap();
        let original_fingerprint = fingerprint(SourceKind::SerializedFile, original_contents);
        let changed_fingerprint = fingerprint(SourceKind::SerializedFile, changed_contents);

        let mut verification_budget = AssetLoadBudget::default();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &path,
            original_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        fs::write(&path, changed_contents).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(original_modified)
            .unwrap();

        let mut revalidation_budget = AssetLoadBudget::default();
        assert!(matches!(
            binding.revalidate_current_contents(&mut revalidation_budget),
            Err(CatalogError::VerifiedFingerprintMismatch { expected, actual })
                if expected == original_fingerprint && actual == changed_fingerprint
        ));
        assert_eq!(revalidation_budget.usage().bytes, 0);
        assert_eq!(revalidation_budget.usage().entries, 0);
    }

    #[test]
    fn unbound_companion_identity_revision_and_locator_are_deterministic() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let root_origin = physical_origin("main.assets", b"asset");
        let member = SourceMemberId::new("main.resS").unwrap();
        let source_fingerprint = fingerprint(SourceKind::StreamedResource, b"resource");

        let mut first = SourceCatalog::new(workspace);
        let first_parent = first
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("main.assets").unwrap(),
                    root_origin.clone(),
                ),
                fingerprint(SourceKind::SerializedFile, b"asset"),
            )
            .unwrap();
        let mut budget = AssetLoadBudget::default();
        let mut transaction = first.begin_transaction(&mut budget).unwrap();
        let first_companion = transaction
            .register_companion(
                first_parent,
                member.clone(),
                source_fingerprint,
                &mut budget,
            )
            .unwrap();
        first = transaction.commit(&mut budget).unwrap();

        let mut second = SourceCatalog::new(workspace);
        let second_parent = second
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("main.assets").unwrap(),
                    root_origin,
                ),
                fingerprint(SourceKind::SerializedFile, b"asset"),
            )
            .unwrap();
        let mut budget = AssetLoadBudget::default();
        let mut transaction = second.begin_transaction(&mut budget).unwrap();
        let second_companion = transaction
            .register_companion(second_parent, member, source_fingerprint, &mut budget)
            .unwrap();
        second = transaction.commit(&mut budget).unwrap();

        assert_eq!(first_parent, second_parent);
        assert_eq!(first_companion, second_companion);
        let unbound_revision = first.revision().unwrap();
        assert_eq!(unbound_revision, second.revision().unwrap());
        assert_eq!(first.parent(first_companion).unwrap(), Some(first_parent));
        assert_eq!(
            first.resolve(first_companion).unwrap().location_kind(),
            SourceLocationKind::Companion
        );
        assert_eq!(
            first
                .source_locator(first_companion)
                .unwrap()
                .members()
                .last()
                .unwrap()
                .container(),
            ContainmentKind::Companion
        );
        assert!(matches!(
            first.physical_origin(first_companion),
            Err(CatalogError::UnboundPhysicalOrigin { source_id })
                if source_id == first_companion
        ));
        assert_eq!(first.physical_origin_option(first_companion).unwrap(), None);
        assert_eq!(
            second.physical_origin_option(second_companion).unwrap(),
            None
        );
        first.validate().unwrap();
        second.validate().unwrap();
    }

    #[test]
    fn borrowed_child_lookup_preserves_container_member_and_binding_invariants() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let root = catalog
            .register(
                root_descriptor(SourceKind::Archive, "game.apk", b"archive"),
                fingerprint(SourceKind::Archive, b"archive"),
            )
            .unwrap();
        let member_id = SourceMemberId::new("main.assets").unwrap();
        let member = catalog
            .register(
                SourceDescriptor::archive_member(
                    root,
                    SourceKind::SerializedFile,
                    member_id.clone(),
                )
                .unwrap(),
                fingerprint(SourceKind::SerializedFile, b"asset"),
            )
            .unwrap();
        let companion_id = SourceMemberId::new("main.resS").unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        let companion = transaction
            .register_companion(
                member,
                companion_id.clone(),
                fingerprint(SourceKind::StreamedResource, b"resource"),
                &mut operation_budget,
            )
            .unwrap();
        catalog = transaction.commit(&mut operation_budget).unwrap();

        let member_step = ContainmentStep::new(ContainmentKind::Archive, member_id.clone());
        let companion_step = ContainmentStep::new(ContainmentKind::Companion, companion_id.clone());
        assert_eq!(
            catalog.child_by_step(root, &member_step).unwrap(),
            Some(member)
        );
        assert_eq!(
            catalog
                .child_by_member(root, ContainmentKind::Archive, &member_id)
                .unwrap(),
            Some(member)
        );
        assert_eq!(
            catalog.child_by_step(member, &companion_step).unwrap(),
            Some(companion)
        );
        assert_eq!(
            catalog
                .child_by_member(member, ContainmentKind::Companion, &companion_id)
                .unwrap(),
            Some(companion)
        );
        assert_eq!(
            catalog
                .child_by_member(root, ContainmentKind::Bundle, &member_id)
                .unwrap(),
            None
        );
        assert_eq!(
            catalog
                .child_by_member(
                    root,
                    ContainmentKind::Archive,
                    &SourceMemberId::with_occurrence("main.assets", 1).unwrap(),
                )
                .unwrap(),
            None
        );

        let root_origin = catalog.physical_origin(root).unwrap();
        assert_eq!(
            catalog.physical_origin_option(root).unwrap(),
            Some(root_origin)
        );
        assert_eq!(
            catalog.physical_origin_option(member).unwrap(),
            Some(root_origin)
        );
        assert_eq!(catalog.physical_origin_option(companion).unwrap(), None);

        let foreign = SourceId::new(
            WorkspaceId::from_u128(2).unwrap(),
            root.kind(),
            root.local(),
        )
        .unwrap();
        assert!(matches!(
            catalog.child_by_step(foreign, &member_step),
            Err(CatalogError::WorkspaceMismatch { .. })
        ));
    }

    #[test]
    fn yaml_sources_can_own_unbound_companions_and_other_parent_kinds_are_rejected() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let yaml = catalog
            .register(
                root_descriptor(SourceKind::Yaml, "scene.yaml", b"yaml"),
                fingerprint(SourceKind::Yaml, b"yaml"),
            )
            .unwrap();
        let companion_id = SourceMemberId::new("scene.resS").unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        let companion = transaction
            .register_companion(
                yaml,
                companion_id.clone(),
                fingerprint(SourceKind::StreamedResource, b"resource"),
                &mut operation_budget,
            )
            .unwrap();
        catalog = transaction.commit(&mut operation_budget).unwrap();

        let locator = catalog.source_locator(companion).unwrap();
        assert_eq!(
            catalog.classify_locator(locator),
            LocatorResolution::Resolved(companion)
        );
        assert_eq!(
            catalog
                .child_by_member(yaml, ContainmentKind::Companion, &companion_id)
                .unwrap(),
            Some(companion)
        );
        assert_eq!(catalog.physical_origin_option(companion).unwrap(), None);
        catalog.validate().unwrap();

        for (ordinal, kind) in [
            SourceKind::Archive,
            SourceKind::WebFile,
            SourceKind::AssetBundle,
            SourceKind::StreamedResource,
        ]
        .into_iter()
        .enumerate()
        {
            let parent = SourceId::new(workspace, kind, ordinal as u128 + 1).unwrap();
            assert!(matches!(
                SourceDescriptor::companion(parent, companion_id.clone()),
                Err(CatalogError::InvalidCompanionParentKind { actual, .. })
                    if actual == kind
            ));
        }
    }

    #[test]
    fn wide_root_removal_visits_and_budgets_only_removed_subtrees() {
        const ROOT_COUNT: usize = 128;

        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let mut roots = Vec::new();
        for ordinal in 0..ROOT_COUNT {
            let alias = format!("root-{ordinal}.assets");
            roots.push(
                catalog
                    .register(
                        root_descriptor(SourceKind::SerializedFile, &alias, alias.as_bytes()),
                        fingerprint(SourceKind::SerializedFile, alias.as_bytes()),
                    )
                    .unwrap(),
            );
        }

        let mut single = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        let mut single_budget = AssetLoadBudget::default();
        single.remove_subtree(roots[0], &mut single_budget).unwrap();
        let single_usage = single_budget.usage();

        let mut measured = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        let mut measured_budget = AssetLoadBudget::default();
        for root in &roots {
            assert_eq!(
                measured
                    .remove_subtree(*root, &mut measured_budget)
                    .unwrap(),
                [*root]
            );
        }
        assert_eq!(measured.subtree_removal_index_visits, ROOT_COUNT * 2);
        let usage = measured_budget.usage();
        assert_eq!(usage.bytes, single_usage.bytes * ROOT_COUNT as u64);

        let mut exact = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        let mut exact_budget = budget_with(usage.bytes, 1);
        for root in &roots {
            exact.remove_subtree(*root, &mut exact_budget).unwrap();
        }
        assert_eq!(exact_budget.usage().bytes, usage.bytes);
        assert!(exact.commit(&mut exact_budget).unwrap().is_empty());

        let mut rejected = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        let mut one_short = budget_with(usage.bytes - 1, 1);
        let mut failed = false;
        for root in &roots {
            if rejected.remove_subtree(*root, &mut one_short).is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed);
        assert!(matches!(
            rejected.commit(&mut one_short),
            Err(CatalogError::TransactionAborted)
        ));
    }

    #[test]
    fn companion_registration_has_exact_combined_budget_and_removes_with_its_subtree() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let root = catalog
            .register(
                root_descriptor(SourceKind::Archive, "game.apk", b"archive"),
                fingerprint(SourceKind::Archive, b"archive"),
            )
            .unwrap();
        let member = catalog
            .register(
                SourceDescriptor::archive_member(
                    root,
                    SourceKind::SerializedFile,
                    SourceMemberId::new("main.assets").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::SerializedFile, b"asset"),
            )
            .unwrap();
        let original_revision = catalog.revision().unwrap();
        let companion_member = SourceMemberId::new("main.resS").unwrap();
        let descriptor = SourceDescriptor::companion(member, companion_member.clone()).unwrap();
        let planned = catalog.checked_registration_bytes(&descriptor).unwrap();

        let mut begin_budget = AssetLoadBudget::default();
        let mut rejected = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut tiny_budget = budget_with(planned - 1, 1);
        assert!(matches!(
            rejected.register_companion(
                member,
                companion_member.clone(),
                fingerprint(SourceKind::StreamedResource, b"resource"),
                &mut tiny_budget,
            ),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(tiny_budget.usage().bytes, 0);
        assert_eq!(tiny_budget.usage().entries, 0);
        assert!(matches!(
            rejected.commit(&mut tiny_budget),
            Err(CatalogError::TransactionAborted)
        ));
        assert_eq!(catalog.revision().unwrap(), original_revision);

        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut exact = budget_with(planned, 1);
        let companion = transaction
            .register_companion(
                member,
                companion_member,
                fingerprint(SourceKind::StreamedResource, b"resource"),
                &mut exact,
            )
            .unwrap();
        catalog = transaction.commit(&mut exact).unwrap();
        assert_eq!(exact.usage().bytes, planned);
        assert_eq!(exact.usage().entries, 1);
        assert!(matches!(
            catalog.physical_origin(companion),
            Err(CatalogError::UnboundPhysicalOrigin { source_id }) if source_id == companion
        ));

        let mut budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut budget).unwrap();
        let removed = transaction.remove_subtree(root, &mut budget).unwrap();
        let candidate = transaction.commit(&mut budget).unwrap();

        assert!(removed.contains(&root));
        assert!(removed.contains(&member));
        assert!(removed.contains(&companion));
        assert!(candidate.is_empty());
        candidate.validate().unwrap();
    }

    #[test]
    fn companion_physical_binding_is_exact_budgeted_atomic_and_indexed() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let root = catalog
            .register(
                root_descriptor(SourceKind::SerializedFile, "main.assets", b"asset"),
                fingerprint(SourceKind::SerializedFile, b"asset"),
            )
            .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut registration = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut registration_budget = AssetLoadBudget::default();
        let companion = registration
            .register_companion(
                root,
                SourceMemberId::new("main.resS").unwrap(),
                fingerprint(SourceKind::StreamedResource, b"resource"),
                &mut registration_budget,
            )
            .unwrap();
        catalog = registration.commit(&mut registration_budget).unwrap();
        let origin = physical_origin("main.resS", b"resource");

        let mut begin_budget = AssetLoadBudget::default();
        let mut measured = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut measurement_budget = AssetLoadBudget::default();
        measured
            .bind_companion_origin(companion, origin.clone(), &mut measurement_budget)
            .unwrap();
        let usage = measurement_budget.usage();
        let measured = measured.commit(&mut measurement_budget).unwrap();
        assert_eq!(measured.find_physical(&origin), Some(companion));
        assert_eq!(measured.physical_origin(companion).unwrap(), &origin);

        let mut begin_budget = AssetLoadBudget::default();
        let mut rejected = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut one_short = budget_with(usage.bytes - 1, usage.entries);
        assert!(matches!(
            rejected.bind_companion_origin(companion, origin.clone(), &mut one_short),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(one_short.usage().bytes, 0);
        assert_eq!(one_short.usage().entries, 0);
        assert!(matches!(
            rejected.commit(&mut one_short),
            Err(CatalogError::TransactionAborted)
        ));
        assert_eq!(catalog.physical_origin_option(companion).unwrap(), None);

        let mut begin_budget = AssetLoadBudget::default();
        let mut exact = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut exact_budget = budget_with(usage.bytes, usage.entries);
        exact
            .bind_companion_origin(companion, origin.clone(), &mut exact_budget)
            .unwrap();
        let exact = exact.commit(&mut exact_budget).unwrap();
        assert_eq!(exact_budget.usage(), usage);
        assert_eq!(exact.find_physical(&origin), Some(companion));
        exact.validate().unwrap();
    }

    #[test]
    fn failed_late_batch_cannot_publish_partial_sources() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let catalog = SourceCatalog::new(workspace);
        let original_revision = catalog.revision().unwrap();
        let mut budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut budget).unwrap();

        let archive = transaction
            .register(
                root_descriptor(SourceKind::Archive, "game.apk", b"archive"),
                fingerprint(SourceKind::Archive, b"archive"),
                &mut budget,
            )
            .unwrap();
        transaction
            .register(
                SourceDescriptor::archive_member(
                    archive,
                    SourceKind::SerializedFile,
                    SourceMemberId::new("first.assets").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::SerializedFile, b"first"),
                &mut budget,
            )
            .unwrap();
        assert!(matches!(
            transaction.register(
                SourceDescriptor::archive_member(
                    archive,
                    SourceKind::Yaml,
                    SourceMemberId::new("second.prefab").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::SerializedFile, b"wrong kind"),
                &mut budget,
            ),
            Err(CatalogError::SourceKindMismatch { .. })
        ));
        assert!(matches!(
            transaction.commit(&mut budget),
            Err(CatalogError::TransactionAborted)
        ));

        assert!(catalog.is_empty());
        assert_eq!(catalog.revision().unwrap(), original_revision);
    }

    #[test]
    fn subtree_removal_uses_parent_ownership_not_path_prefixes() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let root = catalog
            .register(
                root_descriptor(SourceKind::Archive, "content", b"root"),
                fingerprint(SourceKind::Archive, b"root"),
            )
            .unwrap();
        let child = catalog
            .register(
                SourceDescriptor::archive_member(
                    root,
                    SourceKind::SerializedFile,
                    SourceMemberId::new("child.assets").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::SerializedFile, b"child"),
            )
            .unwrap();
        let prefix_sibling = catalog
            .register(
                root_descriptor(
                    SourceKind::SerializedFile,
                    "content/sibling.assets",
                    b"sibling",
                ),
                fingerprint(SourceKind::SerializedFile, b"sibling"),
            )
            .unwrap();

        let mut budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut budget).unwrap();
        let removed = transaction.remove_subtree(root, &mut budget).unwrap();
        let candidate = transaction.commit(&mut budget).unwrap();

        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&root));
        assert!(removed.contains(&child));
        assert!(!candidate.contains(root));
        assert!(!candidate.contains(child));
        assert!(candidate.contains(prefix_sibling));
        candidate.validate().unwrap();
    }

    #[test]
    fn subtree_replacement_is_one_candidate_operation_and_preserves_ids() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let archive = catalog
            .register(
                root_descriptor(SourceKind::Archive, "game.apk", b"old archive"),
                fingerprint(SourceKind::Archive, b"old archive"),
            )
            .unwrap();
        let member_id = SourceMemberId::new("main.assets").unwrap();
        let member = catalog
            .register(
                SourceDescriptor::archive_member(
                    archive,
                    SourceKind::SerializedFile,
                    member_id.clone(),
                )
                .unwrap(),
                fingerprint(SourceKind::SerializedFile, b"old member"),
            )
            .unwrap();
        let old_revision = catalog.revision().unwrap();

        let mut budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut budget).unwrap();
        let inserted = transaction
            .replace_subtree(
                archive,
                [
                    (
                        root_descriptor(SourceKind::Archive, "game.apk", b"new archive"),
                        fingerprint(SourceKind::Archive, b"new archive"),
                    ),
                    (
                        SourceDescriptor::archive_member(
                            archive,
                            SourceKind::SerializedFile,
                            member_id,
                        )
                        .unwrap(),
                        fingerprint(SourceKind::SerializedFile, b"new member"),
                    ),
                ],
                &mut budget,
            )
            .unwrap();
        let candidate = transaction.commit(&mut budget).unwrap();

        assert_eq!(inserted, vec![archive, member]);
        assert_eq!(candidate.children(archive).unwrap(), vec![member]);
        assert_eq!(
            candidate.fingerprint(member).unwrap(),
            fingerprint(SourceKind::SerializedFile, b"new member")
        );
        assert_ne!(candidate.revision().unwrap(), old_revision);
    }

    #[test]
    fn locator_classification_distinguishes_unloaded_missing_and_invalid_paths() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let root = catalog
            .register(
                root_descriptor(SourceKind::Archive, "game.apk", b"archive"),
                fingerprint(SourceKind::Archive, b"archive"),
            )
            .unwrap();
        let child = catalog
            .register(
                SourceDescriptor::archive_member(
                    root,
                    SourceKind::SerializedFile,
                    SourceMemberId::new("main.assets").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::SerializedFile, b"asset"),
            )
            .unwrap();

        let root_locator = catalog.source_locator(root).unwrap().clone();
        let child_locator = catalog.source_locator(child).unwrap().clone();
        let missing = root_locator
            .clone()
            .child(
                ContainmentKind::Archive,
                SourceMemberId::new("missing.assets").unwrap(),
            )
            .unwrap();
        let wrong_container = root_locator
            .clone()
            .child(
                ContainmentKind::Bundle,
                SourceMemberId::new("main.assets").unwrap(),
            )
            .unwrap();
        let below_leaf = child_locator
            .clone()
            .child(
                ContainmentKind::Archive,
                SourceMemberId::new("impossible.assets").unwrap(),
            )
            .unwrap();

        assert_eq!(
            catalog.classify_locator(&root_locator),
            LocatorResolution::Resolved(root)
        );
        assert_eq!(
            catalog.classify_locator(&child_locator),
            LocatorResolution::Resolved(child)
        );
        assert_eq!(
            catalog.classify_locator(&SourceLocator::path("not-loaded").unwrap()),
            LocatorResolution::Unloaded
        );
        assert_eq!(
            catalog.classify_locator(&missing),
            LocatorResolution::Missing
        );
        assert_eq!(
            catalog.classify_locator(&wrong_container),
            LocatorResolution::Invalid
        );
        assert_eq!(
            catalog.classify_locator(&below_leaf),
            LocatorResolution::Invalid
        );
    }

    #[test]
    fn transaction_clone_charges_only_shared_index_backing() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let root = catalog
            .register(
                root_descriptor(
                    SourceKind::Archive,
                    "long/catalog/root/game.apk",
                    b"archive",
                ),
                fingerprint(SourceKind::Archive, b"archive"),
            )
            .unwrap();
        catalog
            .register(
                SourceDescriptor::archive_member(
                    root,
                    SourceKind::SerializedFile,
                    SourceMemberId::new("deep/member/main.assets").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::SerializedFile, b"asset"),
            )
            .unwrap();

        let retained_bytes = catalog.checked_transaction_clone_bytes().unwrap();
        let mut rejected = budget_with(retained_bytes - 1, catalog.len() as u64);
        assert!(matches!(
            catalog.begin_transaction(&mut rejected),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(rejected.usage().bytes, 0);
        assert_eq!(rejected.usage().entries, 0);

        let mut exact = budget_with(retained_bytes, catalog.len() as u64);
        let candidate = catalog
            .begin_transaction(&mut exact)
            .unwrap()
            .commit(&mut exact)
            .unwrap();
        assert_eq!(exact.usage().bytes, retained_bytes);
        assert_eq!(exact.usage().entries, catalog.len() as u64);
        assert_eq!(candidate.revision().unwrap(), catalog.revision().unwrap());
        candidate.validate().unwrap();
    }

    #[test]
    fn root_registration_rejects_before_retained_indexes_allocate() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let descriptor = root_descriptor(SourceKind::Archive, "budgeted/root/game.apk", b"archive");
        let planned = catalog.checked_registration_bytes(&descriptor).unwrap();
        let mut budget = budget_with(planned - 1, 1);

        assert!(matches!(
            catalog.register_impl(
                descriptor,
                fingerprint(SourceKind::Archive, b"archive"),
                Some(&mut budget),
            ),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert!(catalog.is_empty());
        assert_eq!(catalog.by_key.capacity(), 0);
        assert_eq!(catalog.by_locator.capacity(), 0);
        assert_eq!(catalog.physical_bindings.capacity(), 0);
        assert_eq!(catalog.root_aliases.capacity(), 0);
        assert_eq!(budget.usage().bytes, 0);
        assert_eq!(budget.usage().entries, 0);
    }

    #[test]
    fn member_registration_budgets_locator_and_child_indexes_before_mutation() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let root = catalog
            .register(
                root_descriptor(SourceKind::Archive, "game.apk", b"archive"),
                fingerprint(SourceKind::Archive, b"archive"),
            )
            .unwrap();
        let descriptor = SourceDescriptor::archive_member(
            root,
            SourceKind::SerializedFile,
            SourceMemberId::new("nested/path/main.assets").unwrap(),
        )
        .unwrap();
        let planned = catalog.checked_registration_bytes(&descriptor).unwrap();
        let mut budget = budget_with(planned - 1, 1);

        assert!(matches!(
            catalog.register_impl(
                descriptor,
                fingerprint(SourceKind::SerializedFile, b"asset"),
                Some(&mut budget),
            ),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert!(!catalog.children_by_parent.contains_key(&root));
        assert_eq!(budget.usage().bytes, 0);
        assert_eq!(budget.usage().entries, 0);
    }

    #[test]
    fn validation_rejects_empty_or_unowned_child_indexes() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let root = catalog
            .register(
                root_descriptor(SourceKind::SerializedFile, "main.assets", b"asset"),
                fingerprint(SourceKind::SerializedFile, b"asset"),
            )
            .unwrap();
        catalog.children_by_parent.insert(root, HashMap::new());

        assert!(matches!(
            catalog.validate(),
            Err(CatalogError::InvariantUnexpectedChildIndex { parent }) if parent == root
        ));
    }
}
