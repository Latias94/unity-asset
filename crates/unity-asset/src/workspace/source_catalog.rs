use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BundleMemberId, ContainmentKind, ContainmentStep, ContractError,
    DigestBuildError, DigestV1, DigestV1Builder, ObjectAddress, ObjectId, ObjectKind, SourceAlias,
    SourceFingerprint, SourceId, SourceKind, SourceLocator, SourceMemberId, WorkspaceId,
    WorkspaceRevision, arc_value_allocation_bytes,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Runtime filesystem binding. It is never serialized into a logical object address.
pub(crate) struct PhysicalOrigin(PathBuf);

impl PhysicalOrigin {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// Fingerprint observed for one existing source in a physical binding domain.
pub(crate) struct PhysicalDomainSource {
    source: SourceId,
    fingerprint: SourceFingerprint,
}

impl PhysicalDomainSource {
    #[must_use]
    pub(crate) const fn new(source: SourceId, fingerprint: SourceFingerprint) -> Self {
        Self {
            source,
            fingerprint,
        }
    }

    #[must_use]
    pub(crate) const fn source(&self) -> SourceId {
        self.source
    }

    #[must_use]
    pub(crate) const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// New member discovered while rescanning a physical binding domain.
pub(crate) struct PhysicalDomainAddition {
    descriptor: SourceDescriptor,
    fingerprint: SourceFingerprint,
}

impl PhysicalDomainAddition {
    #[must_use]
    pub(crate) const fn new(descriptor: SourceDescriptor, fingerprint: SourceFingerprint) -> Self {
        Self {
            descriptor,
            fingerprint,
        }
    }

    #[must_use]
    pub(crate) const fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    #[must_use]
    pub(crate) const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }
}

#[derive(Debug, Clone, Copy)]
/// Complete observation plus sparse changes for one physical binding domain.
pub(crate) struct PhysicalDomainRewrite<'a> {
    owner: SourceId,
    observed: &'a [PhysicalDomainSource],
    changed: &'a [PhysicalDomainSource],
    additions: &'a [PhysicalDomainAddition],
}

impl<'a> PhysicalDomainRewrite<'a> {
    #[must_use]
    pub(crate) const fn new(
        owner: SourceId,
        observed: &'a [PhysicalDomainSource],
        changed: &'a [PhysicalDomainSource],
        additions: &'a [PhysicalDomainAddition],
    ) -> Self {
        Self {
            owner,
            observed,
            changed,
            additions,
        }
    }

    #[must_use]
    pub(crate) const fn owner(&self) -> SourceId {
        self.owner
    }

    #[must_use]
    pub(crate) const fn observed(&self) -> &'a [PhysicalDomainSource] {
        self.observed
    }

    #[must_use]
    pub(crate) const fn changed(&self) -> &'a [PhysicalDomainSource] {
        self.changed
    }

    #[must_use]
    pub(crate) const fn additions(&self) -> &'a [PhysicalDomainAddition] {
        self.additions
    }
}

#[derive(Debug)]
struct PreparedPhysicalDomainAddition {
    source: SourceId,
    record: Arc<SourceRecord>,
    parent: SourceId,
    step: Arc<ContainmentStep>,
}

#[derive(Debug)]
struct PreparedPhysicalDomainChange {
    source: SourceId,
    record: Arc<SourceRecord>,
}

#[derive(Debug)]
struct PhysicalDomainAdditionPlan {
    additions: Vec<PreparedPhysicalDomainAddition>,
    sorted_parents: Vec<SourceId>,
    scratch_bytes: u64,
    retained_bytes: u64,
}

#[derive(Debug)]
struct PreparedPhysicalDomainRewrite {
    changes: Vec<PreparedPhysicalDomainChange>,
    additions: PhysicalDomainAdditionPlan,
    planned_bytes: u64,
    addition_count: u64,
}

#[derive(Debug)]
struct PreparedPhysicalDomainIndexes {
    by_key: HashMap<Arc<Vec<u8>>, SourceId>,
    by_locator: HashMap<Arc<SourceLocator>, SourceId>,
    children_by_parent: HashMap<SourceId, HashMap<Arc<ContainmentStep>, SourceId>>,
}

impl PhysicalDomainAdditionPlan {
    fn new(addition_count: usize, budget: &AssetLoadBudget) -> Result<Self, CatalogError> {
        let mut scratch_bytes = checked_vec_exact_bytes::<PreparedPhysicalDomainAddition>(
            addition_count,
            "physical domain prepared additions",
        )?;
        scratch_bytes = checked_byte_add(
            scratch_bytes,
            checked_vec_exact_bytes::<SourceId>(
                addition_count,
                "physical domain addition parents",
            )?,
        )?;
        scratch_bytes = checked_byte_add(
            scratch_bytes,
            checked_empty_hash_map_bytes::<SourceId, usize>(addition_count)?,
        )?;
        scratch_bytes = checked_byte_add(
            scratch_bytes,
            checked_empty_hash_map_bytes::<Arc<SourceLocator>, SourceId>(addition_count)?,
        )?;
        budget.check_bytes(scratch_bytes)?;

        let mut additions = Vec::new();
        additions
            .try_reserve_exact(addition_count)
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "physical domain prepared additions",
                requested: addition_count,
                unit: CatalogAllocationUnit::Elements,
                message: error.to_string(),
            })?;
        let mut sorted_parents = Vec::new();
        sorted_parents
            .try_reserve_exact(addition_count)
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "physical domain addition parents",
                requested: addition_count,
                unit: CatalogAllocationUnit::Elements,
                message: error.to_string(),
            })?;
        Ok(Self {
            additions,
            sorted_parents,
            scratch_bytes,
            retained_bytes: 0,
        })
    }

    fn push(
        &mut self,
        addition: PreparedPhysicalDomainAddition,
        retained_bytes: u64,
    ) -> Result<(), CatalogError> {
        self.retained_bytes = checked_byte_add(self.retained_bytes, retained_bytes)?;
        self.sorted_parents.push(addition.parent);
        self.additions.push(addition);
        Ok(())
    }

    fn finish(&mut self) {
        self.sorted_parents.sort_unstable();
    }

    fn parent_runs(&self) -> impl Iterator<Item = (SourceId, usize)> + '_ {
        let mut offset = 0_usize;
        std::iter::from_fn(move || {
            let parent = *self.sorted_parents.get(offset)?;
            let run =
                self.sorted_parents[offset..].partition_point(|candidate| *candidate == parent);
            offset += run;
            Some((parent, run))
        })
    }

    fn additions_for_parent(&self, parent: SourceId) -> usize {
        let first = self
            .sorted_parents
            .partition_point(|candidate| *candidate < parent);
        let last = self
            .sorted_parents
            .partition_point(|candidate| *candidate <= parent);
        last.saturating_sub(first)
    }
}

#[derive(Debug)]
struct SourceRecord {
    descriptor: SourceDescriptor,
    fingerprint: SourceFingerprint,
    source_locator: Arc<SourceLocator>,
    physical_origin: Option<Arc<PhysicalOrigin>>,
    canonical_key: Arc<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhysicalBindingDomain {
    Unbound,
    Direct,
    Inherited,
}

impl PhysicalBindingDomain {
    const fn tag(self) -> u8 {
        match self {
            Self::Unbound => 0,
            Self::Direct => 1,
            Self::Inherited => 2,
        }
    }
}

fn physical_binding_domain(record: &SourceRecord) -> PhysicalBindingDomain {
    match (&record.descriptor.placement, &record.physical_origin) {
        (_, None) => PhysicalBindingDomain::Unbound,
        (SourcePlacement::Root { .. } | SourcePlacement::Companion { .. }, Some(_)) => {
            PhysicalBindingDomain::Direct
        }
        (SourcePlacement::Member { .. }, Some(_)) => PhysicalBindingDomain::Inherited,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PhysicalFileIdentity {
    length: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change_time_seconds: i64,
    #[cfg(unix)]
    change_time_nanoseconds: i64,
    #[cfg(windows)]
    creation_time: u64,
    #[cfg(windows)]
    file_attributes: u32,
    #[cfg(windows)]
    volume_serial_number: u64,
    #[cfg(windows)]
    file_id: [u8; 16],
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
        if expected_fingerprint.kind() != kind {
            return Err(CatalogError::SourceKindMismatch {
                expected: kind,
                actual: expected_fingerprint.kind(),
            });
        }
        let requested = path.as_ref();
        budget.check_bytes(checked_usize_to_u64(requested.as_os_str().len())?)?;
        let physical_origin = PhysicalOrigin::from_existing_path(requested)?;
        let mut file = open_verified_file(physical_origin.path())
            .map_err(|error| CatalogError::verified_binding_io(physical_origin.path(), error))?;
        let before = physical_file_identity(&file, physical_origin.path())?;
        let planned_bytes = checked_byte_add(
            before.length,
            checked_usize_to_u64(physical_origin.path().as_os_str().len())?,
        )?;
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
        if fingerprint != expected_fingerprint {
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

    const fn revalidation_bytes(&self) -> u64 {
        self.file_identity.length
    }

    fn revalidate_current_contents(&self) -> Result<(), CatalogError> {
        revalidate_physical_contents(
            self.kind,
            &self.physical_origin,
            self.fingerprint,
            &self.file_identity,
        )
    }
}

#[derive(Debug)]
struct PendingPhysicalVerification {
    source: SourceId,
    kind: SourceKind,
    fingerprint: SourceFingerprint,
    file_identity: PhysicalFileIdentity,
}

impl PendingPhysicalVerification {
    const fn revalidation_bytes(&self) -> u64 {
        self.file_identity.length
    }

    fn revalidate(&self, catalog: &SourceCatalog) -> Result<(), CatalogError> {
        let record = catalog
            .by_id
            .get(&self.source)
            .ok_or(CatalogError::UnknownSource(self.source))?;
        if record.descriptor.kind != self.kind {
            return Err(CatalogError::SourceKindMismatch {
                expected: self.kind,
                actual: record.descriptor.kind,
            });
        }
        if record.fingerprint != self.fingerprint {
            return Err(CatalogError::VerifiedFingerprintMismatch {
                expected: self.fingerprint,
                actual: record.fingerprint,
            });
        }
        let physical_origin =
            record
                .physical_origin
                .as_deref()
                .ok_or(CatalogError::UnboundPhysicalOrigin {
                    source_id: self.source,
                })?;
        revalidate_physical_contents(
            self.kind,
            physical_origin,
            self.fingerprint,
            &self.file_identity,
        )
    }
}

fn revalidate_physical_contents(
    kind: SourceKind,
    physical_origin: &PhysicalOrigin,
    fingerprint: SourceFingerprint,
    file_identity: &PhysicalFileIdentity,
) -> Result<(), CatalogError> {
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
    if &before != file_identity || before != after || before != path_identity {
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
    Ok(())
}

fn physical_file_identity(
    file: &fs::File,
    path: &Path,
) -> Result<PhysicalFileIdentity, CatalogError> {
    let metadata = file
        .metadata()
        .map_err(|error| CatalogError::verified_binding_io(path, error))?;
    if !metadata.is_file() {
        return Err(CatalogError::VerifiedPhysicalBindingChanged {
            path: path.to_path_buf(),
        });
    }
    let modified = metadata
        .modified()
        .map_err(|error| CatalogError::verified_binding_io(path, error))?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;
    #[cfg(windows)]
    use std::os::windows::fs::MetadataExt as _;
    #[cfg(windows)]
    let (volume_serial_number, file_id) = windows_file_identity(file, path)?;
    Ok(PhysicalFileIdentity {
        length: metadata.len(),
        modified,
        created: metadata.created().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        change_time_seconds: metadata.ctime(),
        #[cfg(unix)]
        change_time_nanoseconds: metadata.ctime_nsec(),
        #[cfg(windows)]
        creation_time: metadata.creation_time(),
        #[cfg(windows)]
        file_attributes: metadata.file_attributes(),
        #[cfg(windows)]
        volume_serial_number,
        #[cfg(windows)]
        file_id,
    })
}

fn physical_file_identity_from_path(path: &Path) -> Result<PhysicalFileIdentity, CatalogError> {
    let file =
        open_verified_file(path).map_err(|error| CatalogError::verified_binding_io(path, error))?;
    physical_file_identity(&file, path)
}

fn open_verified_file(path: &Path) -> io::Result<fs::File> {
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
            pending_verifications: Vec::new(),
            failed: false,
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
                if let Some(anchor) = object.yaml_anchor() {
                    ObjectAddress::yaml(source_locator, anchor)
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
        const PREFIX: &[u8] = b"unity-asset:source-catalog:v5\0";

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
            logical_length = checked_add(logical_length, 1)?;
            if physical_binding_domain(record) == PhysicalBindingDomain::Direct {
                let physical_origin = record.physical_origin.as_deref().ok_or(
                    CatalogError::InvariantRecordMismatch {
                        source_id: *source,
                        field: "direct physical binding",
                    },
                )?;
                logical_length = checked_add(
                    logical_length,
                    DigestV1Builder::framed_len(
                        physical_origin.path().as_os_str().as_encoded_bytes(),
                    )?,
                )?;
            }
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
            let binding_domain = physical_binding_domain(record);
            digest.update(&[binding_domain.tag()])?;
            if binding_domain == PhysicalBindingDomain::Direct {
                let physical_origin = record.physical_origin.as_deref().ok_or(
                    CatalogError::InvariantRecordMismatch {
                        source_id: *source,
                        field: "direct physical binding",
                    },
                )?;
                digest.update_framed(physical_origin.path().as_os_str().as_encoded_bytes())?;
            }
        }
        Ok(WorkspaceRevision::new(digest.finalize()?))
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
    ) -> Result<Vec<SourceId>, CatalogError> {
        self.resolve(root)?;
        let scratch_bytes =
            u64::try_from(self.by_id.len().checked_mul(size_of::<SourceId>()).ok_or(
                CatalogError::AllocationSizeOverflow {
                    resource: "source catalog subtree scratch",
                },
            )?)
            .map_err(|_| CatalogError::AllocationSizeOverflow {
                resource: "source catalog subtree scratch",
            })?;
        budget.check_bytes(scratch_bytes)?;
        let mut removed = Vec::new();
        removed
            .try_reserve_exact(self.by_id.len())
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog subtree scratch",
                requested: self.by_id.len(),
                unit: CatalogAllocationUnit::Elements,
                message: error.to_string(),
            })?;
        budget.consume_bytes(scratch_bytes)?;

        for source in self.by_id.keys().copied() {
            if self.is_owned_by(source, root)? {
                removed.push(source);
            }
        }
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
                    if let Some(children) = self.children_by_parent.get_mut(parent) {
                        children.remove(step);
                    }
                }
            }
        }
        self.children_by_parent
            .retain(|_, children| !children.is_empty());
        Ok(removed)
    }

    fn is_owned_by(&self, source: SourceId, root: SourceId) -> Result<bool, CatalogError> {
        let mut current = source;
        for _ in 0..=self.by_id.len() {
            if current == root {
                return Ok(true);
            }
            let Some(parent) = self
                .by_id
                .get(&current)
                .ok_or(CatalogError::UnknownSource(current))?
                .descriptor
                .parent()
            else {
                return Ok(false);
            };
            current = parent;
        }
        Err(CatalogError::InvariantOwnershipCycle { source_id: source })
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

    fn replace_fingerprint(
        &mut self,
        source: SourceId,
        fingerprint: SourceFingerprint,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), CatalogError> {
        if !self.validate_fingerprint_replacement(source, fingerprint)? {
            return Ok(());
        }
        let record = self
            .by_id
            .get(&source)
            .ok_or(CatalogError::UnknownSource(source))?;

        let retained_bytes = checked_record_clone_bytes(&record.descriptor)?;
        budget.check_bytes(retained_bytes)?;
        budget.consume_bytes(retained_bytes)?;
        let replacement = Arc::new(SourceRecord {
            descriptor: record.descriptor.clone(),
            fingerprint,
            source_locator: Arc::clone(&record.source_locator),
            physical_origin: record.physical_origin.clone(),
            canonical_key: Arc::clone(&record.canonical_key),
        });
        self.by_id.insert(source, replacement);
        Ok(())
    }

    fn validate_fingerprint_replacement(
        &self,
        source: SourceId,
        fingerprint: SourceFingerprint,
    ) -> Result<bool, CatalogError> {
        self.ensure_workspace(source)?;
        let record = self
            .by_id
            .get(&source)
            .ok_or(CatalogError::UnknownSource(source))?;
        if fingerprint.kind() != record.descriptor.kind {
            return Err(CatalogError::SourceKindMismatch {
                expected: record.descriptor.kind,
                actual: fingerprint.kind(),
            });
        }
        if fingerprint == record.fingerprint {
            return Ok(false);
        }
        self.ensure_fingerprint_replacement_allowed(source, fingerprint)?;
        Ok(true)
    }

    fn replace_verified_binding(
        &mut self,
        source: SourceId,
        binding: VerifiedPhysicalBinding,
        budget: &mut AssetLoadBudget,
    ) -> Result<PendingPhysicalVerification, CatalogError> {
        self.ensure_workspace(source)?;
        let record = self
            .by_id
            .get(&source)
            .ok_or(CatalogError::UnknownSource(source))?;
        if !record.descriptor.has_independent_physical_origin() {
            return Err(CatalogError::PhysicalOriginBindingUnsupported {
                source_id: source,
                location: record.descriptor.location_kind(),
            });
        }
        if binding.kind != record.descriptor.kind {
            return Err(CatalogError::SourceKindMismatch {
                expected: record.descriptor.kind,
                actual: binding.kind,
            });
        }
        if binding.fingerprint.kind() != binding.kind {
            return Err(CatalogError::SourceKindMismatch {
                expected: binding.kind,
                actual: binding.fingerprint.kind(),
            });
        }
        self.ensure_fingerprint_replacement_allowed(source, binding.fingerprint)?;
        self.ensure_physical_available(source, &binding.physical_origin)?;
        let is_noop = record.physical_origin.as_deref() == Some(&binding.physical_origin)
            && record.fingerprint == binding.fingerprint;
        let planned_bytes = self.checked_verified_binding_operation_bytes(source, &binding)?;
        budget.check_bytes(planned_bytes)?;
        binding.revalidate_current_contents()?;
        let verification = PendingPhysicalVerification {
            source,
            kind: binding.kind,
            fingerprint: binding.fingerprint,
            file_identity: binding.file_identity.clone(),
        };
        if is_noop {
            budget.consume_bytes(planned_bytes)?;
            return Ok(verification);
        }

        let old_physical_origin = record.physical_origin.clone();
        let affected_count = self.by_id.keys().try_fold(0_usize, |count, candidate| {
            self.is_in_physical_binding_domain(*candidate, source)
                .and_then(|affected| {
                    count.checked_add(usize::from(affected)).ok_or(
                        CatalogError::AllocationSizeOverflow {
                            resource: "source catalog verified binding replacements",
                        },
                    )
                })
        })?;
        if old_physical_origin.is_none() {
            self.physical_bindings.try_reserve(1).map_err(|error| {
                CatalogError::AllocationFailed {
                    resource: "source catalog physical-binding index",
                    requested: 1,
                    unit: CatalogAllocationUnit::Slots,
                    message: error.to_string(),
                }
            })?;
        }
        let mut replacements = Vec::new();
        replacements
            .try_reserve_exact(affected_count)
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog verified binding replacements",
                requested: affected_count,
                unit: CatalogAllocationUnit::Elements,
                message: error.to_string(),
            })?;

        let rebound_origin = Arc::new(binding.physical_origin);
        for (record_source, affected) in &self.by_id {
            if !self.is_in_physical_binding_domain(*record_source, source)? {
                continue;
            }
            let descriptor = if *record_source == source {
                rebound_descriptor(source, &affected.descriptor, rebound_origin.as_ref())?
            } else {
                affected.descriptor.clone()
            };
            replacements.push((
                *record_source,
                Arc::new(SourceRecord {
                    descriptor,
                    fingerprint: if *record_source == source {
                        binding.fingerprint
                    } else {
                        affected.fingerprint
                    },
                    source_locator: Arc::clone(&affected.source_locator),
                    physical_origin: Some(Arc::clone(&rebound_origin)),
                    canonical_key: Arc::clone(&affected.canonical_key),
                }),
            ));
        }

        budget.consume_bytes(planned_bytes)?;
        if let Some(old_physical_origin) = old_physical_origin {
            self.physical_bindings.remove(&old_physical_origin);
        }
        self.physical_bindings
            .insert(Arc::clone(&rebound_origin), source);
        for (record_source, replacement) in replacements {
            self.by_id.insert(record_source, replacement);
        }
        Ok(verification)
    }

    fn checked_verified_binding_operation_bytes(
        &self,
        source: SourceId,
        binding: &VerifiedPhysicalBinding,
    ) -> Result<u64, CatalogError> {
        let record = self
            .by_id
            .get(&source)
            .ok_or(CatalogError::UnknownSource(source))?;
        let replacement_bytes = if record.physical_origin.as_deref()
            == Some(&binding.physical_origin)
            && record.fingerprint == binding.fingerprint
        {
            0
        } else {
            self.checked_verified_binding_replacement_bytes(source, binding)?
        };
        checked_byte_add(binding.revalidation_bytes(), replacement_bytes)
    }

    fn checked_verified_binding_replacement_bytes(
        &self,
        source: SourceId,
        binding: &VerifiedPhysicalBinding,
    ) -> Result<u64, CatalogError> {
        let record = self
            .by_id
            .get(&source)
            .ok_or(CatalogError::UnknownSource(source))?;
        let affected_count = self.by_id.keys().try_fold(0_usize, |count, candidate| {
            self.is_in_physical_binding_domain(*candidate, source)
                .and_then(|affected| {
                    count.checked_add(usize::from(affected)).ok_or(
                        CatalogError::AllocationSizeOverflow {
                            resource: "source catalog verified binding replacements",
                        },
                    )
                })
        })?;
        let mut retained_bytes = checked_vec_exact_bytes::<(SourceId, Arc<SourceRecord>)>(
            affected_count,
            "source catalog verified binding replacements",
        )?;
        retained_bytes = checked_byte_add(
            retained_bytes,
            checked_arc_allocation_bytes::<PhysicalOrigin>()?,
        )?;
        if record.physical_origin.is_none() {
            retained_bytes = checked_byte_add(
                retained_bytes,
                checked_hash_map_growth_bytes(
                    &self.physical_bindings,
                    1,
                    "source catalog physical-binding index",
                )?,
            )?;
        }
        for (record_source, affected) in &self.by_id {
            if !self.is_in_physical_binding_domain(*record_source, source)? {
                continue;
            }
            retained_bytes = checked_byte_add(
                retained_bytes,
                checked_arc_allocation_bytes::<SourceRecord>()?,
            )?;
            retained_bytes = checked_byte_add(
                retained_bytes,
                if *record_source == source {
                    checked_rebound_descriptor_bytes(
                        source,
                        &affected.descriptor,
                        &binding.physical_origin,
                    )?
                } else {
                    checked_descriptor_clone_bytes(&affected.descriptor)?
                },
            )?;
        }
        Ok(retained_bytes)
    }

    fn is_in_physical_binding_domain(
        &self,
        candidate: SourceId,
        owner: SourceId,
    ) -> Result<bool, CatalogError> {
        let mut current = candidate;
        for _ in 0..=self.by_id.len() {
            if current == owner {
                return Ok(true);
            }
            let record = self
                .by_id
                .get(&current)
                .ok_or(CatalogError::UnknownSource(current))?;
            match &record.descriptor.placement {
                SourcePlacement::Member { parent, .. } => current = *parent,
                SourcePlacement::Root { .. } | SourcePlacement::Companion { .. } => {
                    return Ok(false);
                }
            }
        }
        Err(CatalogError::InvariantOwnershipCycle {
            source_id: candidate,
        })
    }

    pub(crate) fn physical_domain_sources(
        &self,
        owner: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<PhysicalDomainSource>, CatalogError> {
        let physical_owner = self.physical_domain_owner(owner)?;
        if physical_owner != owner {
            return Err(CatalogError::PhysicalDomainOwnerRequired {
                source_id: owner,
                physical_owner,
            });
        }

        let source_count = self.by_id.keys().try_fold(0_usize, |count, source| {
            if self.is_in_physical_binding_domain(*source, owner)? {
                count
                    .checked_add(1)
                    .ok_or(CatalogError::AllocationSizeOverflow {
                        resource: "physical domain sources",
                    })
            } else {
                Ok(count)
            }
        })?;
        let entry_count = checked_usize_to_u64(source_count)?;
        let retained_bytes = checked_vec_exact_bytes::<PhysicalDomainSource>(
            source_count,
            "physical domain sources",
        )?;
        budget.check_entries(entry_count)?;
        budget.check_bytes(retained_bytes)?;

        let mut sources = Vec::new();
        sources.try_reserve_exact(source_count).map_err(|error| {
            CatalogError::AllocationFailed {
                resource: "physical domain sources",
                requested: source_count,
                unit: CatalogAllocationUnit::Elements,
                message: error.to_string(),
            }
        })?;
        budget.consume_entries(entry_count)?;
        budget.consume_bytes(retained_bytes)?;
        for (source, record) in &self.by_id {
            if self.is_in_physical_binding_domain(*source, owner)? {
                sources.push(PhysicalDomainSource::new(*source, record.fingerprint));
            }
        }
        Ok(sources)
    }

    fn rewrite_physical_domain(
        &mut self,
        rewrite: PhysicalDomainRewrite<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SourceId>, CatalogError> {
        let prepared = self.prepare_physical_domain_rewrite(rewrite, budget)?;
        let additions = rewrite.additions();
        let addition_plan = prepared.additions;

        let mut added = Vec::new();
        added.try_reserve_exact(additions.len()).map_err(|error| {
            CatalogError::AllocationFailed {
                resource: "physical domain rewrite additions",
                requested: additions.len(),
                unit: CatalogAllocationUnit::Elements,
                message: error.to_string(),
            }
        })?;

        let indexes = (!addition_plan.additions.is_empty())
            .then(|| self.prepare_physical_domain_indexes(&addition_plan))
            .transpose()?;
        budget.consume_entries(prepared.addition_count)?;
        budget.consume_bytes(prepared.planned_bytes)?;

        for change in prepared.changes {
            self.by_id.insert(change.source, change.record);
        }

        for addition in addition_plan.additions {
            let source = addition.source;
            self.by_id.insert(source, addition.record);
            added.push(source);
        }
        if let Some(indexes) = indexes {
            self.by_key = indexes.by_key;
            self.by_locator = indexes.by_locator;
            self.children_by_parent = indexes.children_by_parent;
        }
        Ok(added)
    }

    fn prepare_physical_domain_rewrite(
        &self,
        rewrite: PhysicalDomainRewrite<'_>,
        budget: &AssetLoadBudget,
    ) -> Result<PreparedPhysicalDomainRewrite, CatalogError> {
        self.validate_physical_domain_observation_and_changes(rewrite)?;
        let additions = rewrite.additions();
        let addition_count = checked_usize_to_u64(additions.len())?;
        budget.check_entries(addition_count)?;
        let addition_plan =
            self.prepare_physical_domain_additions(rewrite.owner(), additions, budget)?;

        let addition_bytes =
            checked_byte_add(addition_plan.scratch_bytes, addition_plan.retained_bytes)?;
        let change_scratch_bytes = checked_vec_exact_bytes::<PreparedPhysicalDomainChange>(
            rewrite.changed().len(),
            "physical domain prepared changes",
        )?;
        budget.check_bytes(checked_byte_add(addition_bytes, change_scratch_bytes)?)?;
        let mut changes = Vec::new();
        changes
            .try_reserve_exact(rewrite.changed().len())
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "physical domain prepared changes",
                requested: rewrite.changed().len(),
                unit: CatalogAllocationUnit::Elements,
                message: error.to_string(),
            })?;
        let mut changed_record_bytes = 0_u64;
        for change in rewrite.changed() {
            let source = change.source();
            let record = self
                .by_id
                .get(&source)
                .ok_or(CatalogError::UnknownSource(source))?;
            if record.fingerprint != change.fingerprint() {
                let record_bytes = checked_record_clone_bytes(&record.descriptor)?;
                budget.check_bytes(checked_byte_add(
                    addition_bytes,
                    checked_byte_add(
                        change_scratch_bytes,
                        checked_byte_add(changed_record_bytes, record_bytes)?,
                    )?,
                )?)?;
                changed_record_bytes = checked_byte_add(changed_record_bytes, record_bytes)?;
                changes.push(PreparedPhysicalDomainChange {
                    source,
                    record: Arc::new(SourceRecord {
                        descriptor: record.descriptor.clone(),
                        fingerprint: change.fingerprint(),
                        source_locator: Arc::clone(&record.source_locator),
                        physical_origin: record.physical_origin.clone(),
                        canonical_key: Arc::clone(&record.canonical_key),
                    }),
                });
            }
        }
        let result_bytes = checked_vec_exact_bytes::<SourceId>(
            additions.len(),
            "physical domain rewrite additions",
        )?;
        let planned_bytes = checked_byte_add(
            addition_bytes,
            checked_byte_add(
                checked_byte_add(change_scratch_bytes, changed_record_bytes)?,
                result_bytes,
            )?,
        )?;
        budget.check_bytes(planned_bytes)?;
        Ok(PreparedPhysicalDomainRewrite {
            changes,
            additions: addition_plan,
            planned_bytes,
            addition_count,
        })
    }

    #[cfg(test)]
    fn checked_physical_domain_rewrite_bytes(
        &self,
        rewrite: PhysicalDomainRewrite<'_>,
    ) -> Result<u64, CatalogError> {
        let budget = AssetLoadBudget::default();
        Ok(self
            .prepare_physical_domain_rewrite(rewrite, &budget)?
            .planned_bytes)
    }

    fn validate_physical_domain_observation_and_changes(
        &self,
        rewrite: PhysicalDomainRewrite<'_>,
    ) -> Result<(), CatalogError> {
        let owner = rewrite.owner();
        let observed_sources = rewrite.observed();
        let changed = rewrite.changed();
        let physical_owner = self.physical_domain_owner(owner)?;
        if physical_owner != owner {
            return Err(CatalogError::PhysicalDomainOwnerRequired {
                source_id: owner,
                physical_owner,
            });
        }
        ensure_physical_domain_sources_ordered(observed_sources, "observed")?;
        ensure_physical_domain_sources_ordered(changed, "changed")?;

        let mut observed = observed_sources.iter();
        let mut next_observed = observed.next();
        for (source, record) in &self.by_id {
            if !self.is_in_physical_binding_domain(*source, owner)? {
                continue;
            }
            let Some(candidate) = next_observed else {
                return Err(CatalogError::PhysicalDomainObservationMissing {
                    owner,
                    source_id: *source,
                });
            };
            if candidate.source() < *source {
                return Err(CatalogError::PhysicalDomainObservationUnexpected {
                    owner,
                    source_id: candidate.source(),
                });
            }
            if candidate.source() > *source {
                return Err(CatalogError::PhysicalDomainObservationMissing {
                    owner,
                    source_id: *source,
                });
            }
            if candidate.fingerprint() != record.fingerprint {
                return Err(CatalogError::PhysicalDomainFingerprintMismatch {
                    source_id: *source,
                    expected: candidate.fingerprint(),
                    actual: record.fingerprint,
                });
            }
            next_observed = observed.next();
        }
        if let Some(candidate) = next_observed {
            return Err(CatalogError::PhysicalDomainObservationUnexpected {
                owner,
                source_id: candidate.source(),
            });
        }

        for change in changed {
            let source = change.source();
            let fingerprint = change.fingerprint();
            let physical_owner = self.physical_domain_owner(source)?;
            if physical_owner != owner {
                return Err(CatalogError::PhysicalDomainChangeOutsideDomain {
                    source_id: source,
                    physical_owner,
                });
            }
            let record = self
                .by_id
                .get(&source)
                .ok_or(CatalogError::UnknownSource(source))?;
            if fingerprint.kind() != record.descriptor.kind {
                return Err(CatalogError::SourceKindMismatch {
                    expected: record.descriptor.kind,
                    actual: fingerprint.kind(),
                });
            }
        }

        Ok(())
    }

    fn prepare_physical_domain_additions(
        &self,
        owner: SourceId,
        additions: &[PhysicalDomainAddition],
        budget: &AssetLoadBudget,
    ) -> Result<PhysicalDomainAdditionPlan, CatalogError> {
        for addition in additions {
            let descriptor = addition.descriptor();
            if addition.fingerprint().kind() != descriptor.kind() {
                return Err(CatalogError::SourceKindMismatch {
                    expected: descriptor.kind(),
                    actual: addition.fingerprint().kind(),
                });
            }
            let SourcePlacement::Member { parent, .. } = &descriptor.placement else {
                return Err(CatalogError::PhysicalDomainAdditionRequiresMember {
                    location: descriptor.location_kind(),
                });
            };
            self.ensure_workspace(*parent)?;
        }

        let mut plan = PhysicalDomainAdditionPlan::new(additions.len(), budget)?;
        let mut planned_by_id: HashMap<SourceId, usize> = HashMap::new();
        planned_by_id
            .try_reserve(additions.len())
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "physical domain planned source index",
                requested: additions.len(),
                unit: CatalogAllocationUnit::Slots,
                message: error.to_string(),
            })?;
        let mut planned_by_locator: HashMap<Arc<SourceLocator>, SourceId> = HashMap::new();
        planned_by_locator
            .try_reserve(additions.len())
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "physical domain planned locator index",
                requested: additions.len(),
                unit: CatalogAllocationUnit::Slots,
                message: error.to_string(),
            })?;

        for addition in additions {
            let descriptor = addition.descriptor();
            let fingerprint = addition.fingerprint();
            if fingerprint.kind() != descriptor.kind() {
                return Err(CatalogError::SourceKindMismatch {
                    expected: descriptor.kind(),
                    actual: fingerprint.kind(),
                });
            }
            let SourcePlacement::Member { parent, step, .. } = &descriptor.placement else {
                return Err(CatalogError::PhysicalDomainAdditionRequiresMember {
                    location: descriptor.location_kind(),
                });
            };
            self.ensure_workspace(*parent)?;
            let planned_parent = planned_by_id.get(parent).copied();
            let parent_record = if let Some(index) = planned_parent {
                &plan.additions[index].record
            } else {
                self.by_id
                    .get(parent)
                    .ok_or(CatalogError::PhysicalDomainAdditionParentNotReady { parent: *parent })?
            };
            let physical_owner = if planned_parent.is_some() {
                owner
            } else {
                self.physical_domain_owner(*parent)?
            };
            if physical_owner != owner {
                return Err(CatalogError::PhysicalDomainAdditionOutsideDomain {
                    parent: *parent,
                    physical_owner,
                });
            }
            if !valid_member_placement(
                parent_record.descriptor.kind,
                descriptor.kind,
                step,
                descriptor.location_kind(),
            ) {
                return Err(CatalogError::PhysicalDomainAdditionInvalidPlacement {
                    parent: *parent,
                    parent_kind: parent_record.descriptor.kind,
                    child_kind: descriptor.kind,
                    location: descriptor.location_kind(),
                });
            }
            if let Some(existing) = self
                .children_by_parent
                .get(parent)
                .and_then(|children| children.get(step))
                .copied()
            {
                return Err(CatalogError::PhysicalDomainAdditionAlreadyExists {
                    source_id: existing,
                });
            }

            let retained_bytes =
                checked_physical_domain_addition_intrinsic_bytes(parent_record, descriptor)?;
            budget.check_bytes(checked_byte_add(
                plan.scratch_bytes,
                checked_byte_add(plan.retained_bytes, retained_bytes)?,
            )?)?;

            let source_locator = parent_record
                .source_locator
                .as_ref()
                .clone()
                .child(step.container(), step.member().clone())
                .map_err(CatalogError::InvalidIdentity)?;
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
            if let Some(existing) = planned_by_id.get(&source) {
                let existing_record = &plan.additions[*existing].record;
                if existing_record.source_locator.as_ref() == &source_locator {
                    return Err(CatalogError::PhysicalDomainAdditionAlreadyExists {
                        source_id: source,
                    });
                }
                return Err(CatalogError::IdentityCollision {
                    source_id: source,
                    existing_kind: existing_record.descriptor.kind,
                });
            }
            if let Some(existing) = self.by_locator.get(&source_locator) {
                return Err(CatalogError::LocatorCollision {
                    existing: *existing,
                    incoming: source,
                });
            }

            let source_locator = Arc::new(source_locator);
            if let Some(existing) = planned_by_locator.get(&source_locator) {
                return Err(CatalogError::PhysicalDomainAdditionAlreadyExists {
                    source_id: *existing,
                });
            }
            let key = Arc::new(key);
            let record = Arc::new(SourceRecord {
                descriptor: descriptor.clone(),
                fingerprint,
                source_locator: Arc::clone(&source_locator),
                physical_origin: parent_record.physical_origin.clone(),
                canonical_key: key,
            });
            let index = plan.additions.len();
            plan.push(
                PreparedPhysicalDomainAddition {
                    source,
                    record,
                    parent: *parent,
                    step: Arc::new(step.clone()),
                },
                retained_bytes,
            )?;
            planned_by_id.insert(source, index);
            planned_by_locator.insert(source_locator, source);
        }
        plan.finish();

        if !additions.is_empty() {
            let final_source_count = self.by_id.len().checked_add(additions.len()).ok_or(
                CatalogError::AllocationSizeOverflow {
                    resource: "source catalog source indexes",
                },
            )?;
            plan.retained_bytes = checked_byte_add(
                plan.retained_bytes,
                checked_hash_table_bytes::<Arc<Vec<u8>>, SourceId>(
                    final_source_count,
                    "source catalog prepared key index",
                )?,
            )?;
            plan.retained_bytes = checked_byte_add(
                plan.retained_bytes,
                checked_hash_table_bytes::<Arc<SourceLocator>, SourceId>(
                    final_source_count,
                    "source catalog prepared locator index",
                )?,
            )?;
            let mut new_child_index_count = 0_usize;
            let mut child_index_bytes = 0_u64;
            for (parent, children) in &self.children_by_parent {
                let child_count = children
                    .len()
                    .checked_add(plan.additions_for_parent(*parent))
                    .ok_or(CatalogError::AllocationSizeOverflow {
                        resource: "source catalog prepared child-step index",
                    })?;
                child_index_bytes = checked_byte_add(
                    child_index_bytes,
                    checked_hash_table_bytes::<Arc<ContainmentStep>, SourceId>(
                        child_count,
                        "source catalog prepared child-step index",
                    )?,
                )?;
            }
            for (parent, child_count) in plan.parent_runs() {
                if !self.children_by_parent.contains_key(&parent) {
                    new_child_index_count = new_child_index_count.checked_add(1).ok_or(
                        CatalogError::AllocationSizeOverflow {
                            resource: "source catalog child index",
                        },
                    )?;
                    child_index_bytes = checked_byte_add(
                        child_index_bytes,
                        checked_hash_table_bytes::<Arc<ContainmentStep>, SourceId>(
                            child_count,
                            "source catalog prepared child-step index",
                        )?,
                    )?;
                }
            }
            plan.retained_bytes = checked_byte_add(plan.retained_bytes, child_index_bytes)?;
            let final_parent_count = self
                .children_by_parent
                .len()
                .checked_add(new_child_index_count)
                .ok_or(CatalogError::AllocationSizeOverflow {
                    resource: "source catalog prepared child index",
                })?;
            plan.retained_bytes = checked_byte_add(
                plan.retained_bytes,
                checked_hash_table_bytes::<SourceId, HashMap<Arc<ContainmentStep>, SourceId>>(
                    final_parent_count,
                    "source catalog prepared child index",
                )?,
            )?;
        }
        Ok(plan)
    }

    fn prepare_physical_domain_indexes(
        &self,
        plan: &PhysicalDomainAdditionPlan,
    ) -> Result<PreparedPhysicalDomainIndexes, CatalogError> {
        let final_source_count = self.by_id.len().checked_add(plan.additions.len()).ok_or(
            CatalogError::AllocationSizeOverflow {
                resource: "source catalog prepared source indexes",
            },
        )?;
        let mut by_key = HashMap::new();
        by_key
            .try_reserve(final_source_count)
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog prepared key index",
                requested: final_source_count,
                unit: CatalogAllocationUnit::Slots,
                message: error.to_string(),
            })?;
        by_key.extend(
            self.by_key
                .iter()
                .map(|(key, source)| (Arc::clone(key), *source)),
        );

        let mut by_locator = HashMap::new();
        by_locator
            .try_reserve(final_source_count)
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog prepared locator index",
                requested: final_source_count,
                unit: CatalogAllocationUnit::Slots,
                message: error.to_string(),
            })?;
        by_locator.extend(
            self.by_locator
                .iter()
                .map(|(locator, source)| (Arc::clone(locator), *source)),
        );

        let new_parent_count = plan
            .parent_runs()
            .filter(|(parent, _)| !self.children_by_parent.contains_key(parent))
            .count();
        let final_parent_count = self
            .children_by_parent
            .len()
            .checked_add(new_parent_count)
            .ok_or(CatalogError::AllocationSizeOverflow {
                resource: "source catalog prepared child index",
            })?;
        let mut children_by_parent = HashMap::new();
        children_by_parent
            .try_reserve(final_parent_count)
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog prepared child index",
                requested: final_parent_count,
                unit: CatalogAllocationUnit::Slots,
                message: error.to_string(),
            })?;
        for (parent, current_children) in &self.children_by_parent {
            let final_child_count = current_children
                .len()
                .checked_add(plan.additions_for_parent(*parent))
                .ok_or(CatalogError::AllocationSizeOverflow {
                    resource: "source catalog prepared child-step index",
                })?;
            let mut children = HashMap::new();
            children.try_reserve(final_child_count).map_err(|error| {
                CatalogError::AllocationFailed {
                    resource: "source catalog prepared child-step index",
                    requested: final_child_count,
                    unit: CatalogAllocationUnit::Slots,
                    message: error.to_string(),
                }
            })?;
            children.extend(
                current_children
                    .iter()
                    .map(|(step, source)| (Arc::clone(step), *source)),
            );
            children_by_parent.insert(*parent, children);
        }
        for (parent, child_count) in plan.parent_runs() {
            if children_by_parent.contains_key(&parent) {
                continue;
            }
            let mut children = HashMap::new();
            children
                .try_reserve(child_count)
                .map_err(|error| CatalogError::AllocationFailed {
                    resource: "source catalog prepared child-step index",
                    requested: child_count,
                    unit: CatalogAllocationUnit::Slots,
                    message: error.to_string(),
                })?;
            children_by_parent.insert(parent, children);
        }

        for addition in &plan.additions {
            by_key.insert(Arc::clone(&addition.record.canonical_key), addition.source);
            by_locator.insert(Arc::clone(&addition.record.source_locator), addition.source);
            let children = children_by_parent.get_mut(&addition.parent).ok_or(
                CatalogError::InvariantMissingChildIndex {
                    parent: addition.parent,
                },
            )?;
            children.insert(Arc::clone(&addition.step), addition.source);
        }
        Ok(PreparedPhysicalDomainIndexes {
            by_key,
            by_locator,
            children_by_parent,
        })
    }

    fn ensure_fingerprint_replacement_allowed(
        &self,
        source: SourceId,
        fingerprint: SourceFingerprint,
    ) -> Result<(), CatalogError> {
        let record = self
            .by_id
            .get(&source)
            .ok_or(CatalogError::UnknownSource(source))?;
        if fingerprint == record.fingerprint {
            return Ok(());
        }
        if matches!(&record.descriptor.placement, SourcePlacement::Member { .. }) {
            return Err(CatalogError::InheritedSourceReplacementRequired {
                source_id: source,
                physical_owner: self.physical_domain_owner(source)?,
            });
        }
        for candidate in self.by_id.keys().copied() {
            if candidate != source && self.is_in_physical_binding_domain(candidate, source)? {
                return Err(CatalogError::SubtreeReplacementRequired { source_id: source });
            }
        }
        Ok(())
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
    pending_verifications: Vec<PendingPhysicalVerification>,
    failed: bool,
}

impl SourceCatalogTransaction {
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

    pub(crate) fn replace_fingerprint(
        &mut self,
        source: SourceId,
        fingerprint: SourceFingerprint,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), CatalogError> {
        self.ensure_active()?;
        if let Err(error) = self
            .candidate
            .validate_fingerprint_replacement(source, fingerprint)
        {
            self.failed = true;
            return Err(error);
        }
        if let Some(verification) = self.pending_verifications.iter().find(|verification| {
            verification.source == source && verification.fingerprint != fingerprint
        }) {
            self.failed = true;
            return Err(CatalogError::PendingPhysicalVerificationSuperseded {
                source_id: source,
                verified: verification.fingerprint,
                replacement: fingerprint,
            });
        }
        match self
            .candidate
            .replace_fingerprint(source, fingerprint, budget)
        {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    pub(crate) fn rewrite_physical_domain(
        &mut self,
        rewrite: PhysicalDomainRewrite<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SourceId>, CatalogError> {
        self.ensure_active()?;
        if let Some((verification, replacement)) =
            self.pending_verifications.iter().find_map(|verification| {
                rewrite.changed().iter().find_map(|change| {
                    (change.source() == verification.source
                        && change.fingerprint() != verification.fingerprint)
                        .then_some((verification, change.fingerprint()))
                })
            })
        {
            self.failed = true;
            return Err(CatalogError::PendingPhysicalVerificationSuperseded {
                source_id: verification.source,
                verified: verification.fingerprint,
                replacement,
            });
        }
        match self.candidate.rewrite_physical_domain(rewrite, budget) {
            Ok(added) => Ok(added),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    pub(crate) fn replace_verified_binding(
        &mut self,
        source: SourceId,
        binding: VerifiedPhysicalBinding,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), CatalogError> {
        self.ensure_active()?;
        let result = (|| {
            let existing = self
                .pending_verifications
                .iter()
                .position(|verification| verification.source == source);
            let storage_bytes = if existing.is_none()
                && self.pending_verifications.len() == self.pending_verifications.capacity()
            {
                checked_vec_growth_bytes::<PendingPhysicalVerification>(
                    self.pending_verifications.len(),
                    1,
                )?
            } else {
                0
            };
            let apply_bytes = self
                .candidate
                .checked_verified_binding_operation_bytes(source, &binding)?;
            let planned_bytes = checked_byte_add(apply_bytes, storage_bytes)?;
            budget.check_bytes(planned_bytes)?;

            let verification = self
                .candidate
                .replace_verified_binding(source, binding, budget)?;
            if storage_bytes != 0 {
                self.pending_verifications
                    .try_reserve_exact(1)
                    .map_err(|error| CatalogError::AllocationFailed {
                        resource: "source catalog pending physical verification",
                        requested: 1,
                        unit: CatalogAllocationUnit::Elements,
                        message: error.to_string(),
                    })?;
                budget.consume_bytes(storage_bytes)?;
            }
            match existing {
                Some(index) => self.pending_verifications[index] = verification,
                None => self.pending_verifications.push(verification),
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    pub(crate) fn remove_subtree(
        &mut self,
        root: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SourceId>, CatalogError> {
        self.ensure_active()?;
        match self.candidate.remove_subtree(root, budget) {
            Ok(removed) => {
                self.pending_verifications
                    .retain(|verification| self.candidate.contains(verification.source));
                Ok(removed)
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
        if let Err(error) = self.candidate.remove_subtree(root, budget) {
            self.failed = true;
            return Err(error);
        }
        self.pending_verifications
            .retain(|verification| self.candidate.contains(verification.source));

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
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceCatalog, CatalogError> {
        if self.failed {
            return Err(CatalogError::TransactionAborted);
        }
        self.candidate.validate()?;
        let revalidation_bytes = self.checked_commit_revalidation_bytes()?;
        budget.check_bytes(revalidation_bytes)?;
        for verification in &self.pending_verifications {
            verification.revalidate(&self.candidate)?;
        }
        budget.consume_bytes(revalidation_bytes)?;
        Ok(self.candidate)
    }

    fn checked_commit_revalidation_bytes(&self) -> Result<u64, CatalogError> {
        self.pending_verifications
            .iter()
            .try_fold(0_u64, |total, verification| {
                checked_byte_add(total, verification.revalidation_bytes())
            })
    }

    #[cfg(test)]
    fn checked_verified_binding_apply_bytes(
        &self,
        source: SourceId,
        binding: &VerifiedPhysicalBinding,
    ) -> Result<u64, CatalogError> {
        let storage_bytes = if self
            .pending_verifications
            .iter()
            .any(|verification| verification.source == source)
            || self.pending_verifications.len() < self.pending_verifications.capacity()
        {
            0
        } else {
            checked_vec_growth_bytes::<PendingPhysicalVerification>(
                self.pending_verifications.len(),
                1,
            )?
        };
        checked_byte_add(
            self.candidate
                .checked_verified_binding_operation_bytes(source, binding)?,
            storage_bytes,
        )
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
    #[error("source {source_id:?} has member descendants and requires atomic subtree replacement")]
    SubtreeReplacementRequired { source_id: SourceId },
    #[error(
        "source {source_id:?} inherits physical contents from {physical_owner:?} and must be replaced through that owner's subtree"
    )]
    InheritedSourceReplacementRequired {
        source_id: SourceId,
        physical_owner: SourceId,
    },
    #[error(
        "source {source_id:?} belongs to physical domain {physical_owner:?} and cannot own this rewrite"
    )]
    PhysicalDomainOwnerRequired {
        source_id: SourceId,
        physical_owner: SourceId,
    },
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
    #[error("source {source_id:?} belongs to a different physical domain {physical_owner:?}")]
    PhysicalDomainChangeOutsideDomain {
        source_id: SourceId,
        physical_owner: SourceId,
    },
    #[error("physical-domain additions must be inherited members, got {location:?}")]
    PhysicalDomainAdditionRequiresMember { location: SourceLocationKind },
    #[error("addition parent {parent:?} belongs to a different physical domain {physical_owner:?}")]
    PhysicalDomainAdditionOutsideDomain {
        parent: SourceId,
        physical_owner: SourceId,
    },
    #[error("physical-domain addition already exists as source {source_id:?}")]
    PhysicalDomainAdditionAlreadyExists { source_id: SourceId },
    #[error(
        "physical-domain addition parent {parent:?} must exist or precede its children in the batch"
    )]
    PhysicalDomainAdditionParentNotReady { parent: SourceId },
    #[error(
        "physical-domain addition under {parent:?} ({parent_kind:?}) cannot place {child_kind:?} at {location:?}"
    )]
    PhysicalDomainAdditionInvalidPlacement {
        parent: SourceId,
        parent_kind: SourceKind,
        child_kind: SourceKind,
        location: SourceLocationKind,
    },
    #[error(
        "source {source_id:?} has a pending proof for {verified:?} that cannot be superseded by {replacement:?}"
    )]
    PendingPhysicalVerificationSuperseded {
        source_id: SourceId,
        verified: SourceFingerprint,
        replacement: SourceFingerprint,
    },
    #[error("source {source_id:?} at {location:?} cannot own an independent physical origin")]
    PhysicalOriginBindingUnsupported {
        source_id: SourceId,
        location: SourceLocationKind,
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

fn checked_physical_domain_addition_intrinsic_bytes(
    parent: &SourceRecord,
    descriptor: &SourceDescriptor,
) -> Result<u64, CatalogError> {
    let SourcePlacement::Member { step, .. } = &descriptor.placement else {
        return Err(CatalogError::PhysicalDomainAdditionRequiresMember {
            location: descriptor.location_kind(),
        });
    };
    let locator_clone_bytes = parent.source_locator.retained_clone_bytes().ok_or(
        CatalogError::AllocationSizeOverflow {
            resource: "physical domain source locator",
        },
    )?;
    let mut bytes = checked_usize_to_u64(locator_clone_bytes)?;
    bytes = checked_byte_add(
        bytes,
        checked_usize_to_u64(step.member().retained_clone_bytes())?,
    )?;
    bytes = checked_byte_add(
        bytes,
        checked_vec_growth_bytes::<ContainmentStep>(parent.source_locator.members().len(), 1)?,
    )?;
    bytes = checked_byte_add(
        bytes,
        checked_usize_to_u64(canonical_source_key_len_parts(
            descriptor.kind,
            parent.source_locator.root_alias(),
            parent.source_locator.members(),
            Some(step),
        )?)?,
    )?;
    bytes = checked_byte_add(bytes, checked_descriptor_clone_bytes(descriptor)?)?;
    bytes = checked_byte_add(bytes, checked_arc_allocation_bytes::<SourceLocator>()?)?;
    bytes = checked_byte_add(bytes, checked_arc_allocation_bytes::<Vec<u8>>()?)?;
    bytes = checked_byte_add(bytes, checked_arc_allocation_bytes::<SourceRecord>()?)?;
    bytes = checked_byte_add(
        bytes,
        checked_btree_entry_bytes::<SourceId, Arc<SourceRecord>>()?,
    )?;
    bytes = checked_byte_add(bytes, checked_arc_allocation_bytes::<ContainmentStep>()?)?;
    checked_byte_add(
        bytes,
        checked_usize_to_u64(step.member().retained_clone_bytes())?,
    )
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

fn checked_rebound_descriptor_bytes(
    source: SourceId,
    descriptor: &SourceDescriptor,
    physical_origin: &PhysicalOrigin,
) -> Result<u64, CatalogError> {
    match &descriptor.placement {
        SourcePlacement::Root { alias, .. } => checked_byte_add(
            checked_usize_to_u64(alias.retained_clone_bytes())?,
            checked_usize_to_u64(physical_origin.path().as_os_str().len())?,
        ),
        SourcePlacement::Companion { step, .. } => {
            checked_usize_to_u64(step.member().retained_clone_bytes())
        }
        SourcePlacement::Member { .. } => Err(CatalogError::PhysicalOriginBindingUnsupported {
            source_id: source,
            location: descriptor.location_kind(),
        }),
    }
}

fn rebound_descriptor(
    source: SourceId,
    descriptor: &SourceDescriptor,
    physical_origin: &PhysicalOrigin,
) -> Result<SourceDescriptor, CatalogError> {
    let placement = match &descriptor.placement {
        SourcePlacement::Root { alias, .. } => SourcePlacement::Root {
            alias: alias.clone(),
            physical_origin: physical_origin.clone(),
        },
        SourcePlacement::Companion { parent, step } => SourcePlacement::Companion {
            parent: *parent,
            step: step.clone(),
        },
        SourcePlacement::Member { .. } => {
            return Err(CatalogError::PhysicalOriginBindingUnsupported {
                source_id: source,
                location: descriptor.location_kind(),
            });
        }
    };
    Ok(SourceDescriptor {
        kind: descriptor.kind,
        placement,
    })
}

fn checked_vec_exact_bytes<T>(count: usize, resource: &'static str) -> Result<u64, CatalogError> {
    count
        .checked_mul(size_of::<T>())
        .ok_or(CatalogError::AllocationSizeOverflow { resource })
        .and_then(checked_usize_to_u64)
}

fn ensure_physical_domain_sources_ordered(
    sources: &[PhysicalDomainSource],
    collection: &'static str,
) -> Result<(), CatalogError> {
    for pair in sources.windows(2) {
        if pair[0].source() >= pair[1].source() {
            return Err(CatalogError::PhysicalDomainSourcesNotStrictlyOrdered {
                collection,
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

    fn catalog_index_capacities(
        catalog: &SourceCatalog,
    ) -> (usize, usize, usize, Vec<(SourceId, usize)>) {
        let mut child_capacities = catalog
            .children_by_parent
            .iter()
            .map(|(parent, children)| (*parent, children.capacity()))
            .collect::<Vec<_>>();
        child_capacities.sort_unstable_by_key(|(parent, _)| *parent);
        (
            catalog.by_key.capacity(),
            catalog.by_locator.capacity(),
            catalog.children_by_parent.capacity(),
            child_capacities,
        )
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

    fn predicted_source(catalog: &SourceCatalog, descriptor: &SourceDescriptor) -> SourceId {
        let (locator, _) = catalog.resolve_placement(descriptor).unwrap();
        let key = canonical_source_key(descriptor.kind(), &locator).unwrap();
        SourceId::new(
            catalog.workspace(),
            descriptor.kind(),
            deterministic_local_id(&key),
        )
        .unwrap()
    }

    #[test]
    fn physical_domain_enumeration_is_complete_budgeted_and_excludes_companions() {
        let (catalog, root, webfile, serialized_file, companion) = physical_domain_fixture();
        let exact_bytes =
            checked_vec_exact_bytes::<PhysicalDomainSource>(3, "physical domain sources").unwrap();
        let mut exact_budget = budget_with(exact_bytes, 3);
        let sources = catalog
            .physical_domain_sources(root, &mut exact_budget)
            .unwrap();

        let mut expected = vec![root, webfile, serialized_file];
        expected.sort_unstable();
        assert_eq!(
            sources
                .iter()
                .map(PhysicalDomainSource::source)
                .collect::<Vec<_>>(),
            expected
        );
        for source in &sources {
            assert_eq!(
                source.fingerprint(),
                catalog.fingerprint(source.source()).unwrap()
            );
        }
        assert!(!sources.iter().any(|source| source.source() == companion));
        assert_eq!(catalog.physical_domain_owner(companion).unwrap(), companion);
        assert_eq!(exact_budget.usage().entries, 3);
        assert_eq!(exact_budget.usage().bytes, exact_bytes);

        let mut tiny_budget = budget_with(exact_bytes - 1, 3);
        assert!(matches!(
            catalog.physical_domain_sources(root, &mut tiny_budget),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(tiny_budget.usage().entries, 0);
        assert_eq!(tiny_budget.usage().bytes, 0);

        let mut owner_budget = AssetLoadBudget::default();
        assert!(matches!(
            catalog.physical_domain_sources(webfile, &mut owner_budget),
            Err(CatalogError::PhysicalDomainOwnerRequired {
                source_id,
                physical_owner,
            }) if source_id == webfile && physical_owner == root
        ));
        assert_eq!(owner_budget.usage().entries, 0);
        assert_eq!(owner_budget.usage().bytes, 0);
    }

    #[test]
    fn physical_domain_rewrite_applies_sparse_changes_and_additions_atomically() {
        let (catalog, root, webfile, serialized_file, companion) = physical_domain_fixture();
        let companion_fingerprint = catalog.fingerprint(companion).unwrap();
        let webfile_fingerprint = catalog.fingerprint(webfile).unwrap();
        let original_revision = catalog.revision().unwrap();
        let mut observation_budget = AssetLoadBudget::default();
        let observed = catalog
            .physical_domain_sources(root, &mut observation_budget)
            .unwrap();
        let mut changed = vec![
            PhysicalDomainSource::new(root, fingerprint(SourceKind::Archive, b"changed archive")),
            PhysicalDomainSource::new(
                serialized_file,
                fingerprint(SourceKind::SerializedFile, b"changed asset"),
            ),
        ];
        changed.sort_unstable_by_key(PhysicalDomainSource::source);
        let addition = PhysicalDomainAddition::new(
            SourceDescriptor::sidecar(root, SourceMemberId::new("shared.resS").unwrap()).unwrap(),
            fingerprint(SourceKind::StreamedResource, b"shared resource"),
        );
        let additions = [addition];

        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let rewrite = PhysicalDomainRewrite::new(root, &observed, &changed, &additions);
        let exact_bytes = transaction
            .candidate
            .checked_physical_domain_rewrite_bytes(rewrite)
            .unwrap();
        let mut operation_budget = budget_with(exact_bytes, 1);
        let added = transaction
            .rewrite_physical_domain(rewrite, &mut operation_budget)
            .unwrap();
        assert_eq!(added.len(), 1);
        assert_eq!(operation_budget.usage().entries, 1);
        assert_eq!(operation_budget.usage().bytes, exact_bytes);
        let candidate = transaction.commit(&mut operation_budget).unwrap();
        let added_source = added[0];

        assert_eq!(
            candidate.fingerprint(root).unwrap(),
            fingerprint(SourceKind::Archive, b"changed archive")
        );
        assert_eq!(candidate.fingerprint(webfile).unwrap(), webfile_fingerprint);
        assert_eq!(
            candidate.fingerprint(serialized_file).unwrap(),
            fingerprint(SourceKind::SerializedFile, b"changed asset")
        );
        assert_eq!(
            candidate.fingerprint(added_source).unwrap(),
            additions[0].fingerprint()
        );
        assert_eq!(candidate.physical_domain_owner(added_source).unwrap(), root);
        assert_eq!(
            candidate.physical_domain_owner(companion).unwrap(),
            companion
        );
        assert_eq!(
            candidate.fingerprint(companion).unwrap(),
            companion_fingerprint
        );
        assert!(candidate.contains(companion));
        assert_ne!(candidate.revision().unwrap(), original_revision);
        assert_eq!(catalog.revision().unwrap(), original_revision);
        candidate.validate().unwrap();
    }

    #[test]
    fn physical_domain_rewrite_adds_new_containers_before_their_children_atomically() {
        let (catalog, root, _, _, _) = physical_domain_fixture();
        let mut observation_budget = AssetLoadBudget::default();
        let observed = catalog
            .physical_domain_sources(root, &mut observation_budget)
            .unwrap();
        let container_descriptor = SourceDescriptor::archive_member(
            root,
            SourceKind::AssetBundle,
            SourceMemberId::new("nested.bundle").unwrap(),
        )
        .unwrap();
        let container = predicted_source(&catalog, &container_descriptor);
        let child_descriptor = SourceDescriptor::bundle_member(
            container,
            SourceKind::SerializedFile,
            SourceMemberId::new("nested.assets").unwrap(),
        )
        .unwrap();
        let additions = [
            PhysicalDomainAddition::new(
                container_descriptor,
                fingerprint(SourceKind::AssetBundle, b"nested bundle"),
            ),
            PhysicalDomainAddition::new(
                child_descriptor,
                fingerprint(SourceKind::SerializedFile, b"nested assets"),
            ),
        ];
        let rewrite = PhysicalDomainRewrite::new(root, &observed, &[], &additions);

        let mut begin_budget = AssetLoadBudget::default();
        let mut rejected = catalog.begin_transaction(&mut begin_budget).unwrap();
        let planned = rejected
            .candidate
            .checked_physical_domain_rewrite_bytes(rewrite)
            .unwrap();
        let rejected_revision = rejected.candidate.revision().unwrap();
        let mut one_short = budget_with(planned - 1, 2);
        assert!(matches!(
            rejected.rewrite_physical_domain(rewrite, &mut one_short),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(one_short.usage(), Default::default());
        assert_eq!(rejected.candidate.revision().unwrap(), rejected_revision);
        assert!(matches!(
            rejected.commit(&mut one_short),
            Err(CatalogError::TransactionAborted)
        ));

        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut exact = budget_with(planned, 2);
        let added = transaction
            .rewrite_physical_domain(rewrite, &mut exact)
            .unwrap();
        assert_eq!(exact.usage().entries, 2);
        assert_eq!(exact.usage().bytes, planned);
        assert_eq!(added[0], container);
        let child = added[1];
        let candidate = transaction.commit(&mut exact).unwrap();
        assert_eq!(candidate.parent(container).unwrap(), Some(root));
        assert_eq!(candidate.parent(child).unwrap(), Some(container));
        assert_eq!(candidate.physical_domain_owner(child).unwrap(), root);
        candidate.validate().unwrap();
    }

    #[test]
    fn physical_domain_rewrite_rejects_child_before_parent_and_wrong_kind_without_charging() {
        let (catalog, root, _, _, _) = physical_domain_fixture();
        let mut observation_budget = AssetLoadBudget::default();
        let observed = catalog
            .physical_domain_sources(root, &mut observation_budget)
            .unwrap();
        let container_descriptor = SourceDescriptor::archive_member(
            root,
            SourceKind::AssetBundle,
            SourceMemberId::new("nested.bundle").unwrap(),
        )
        .unwrap();
        let container = predicted_source(&catalog, &container_descriptor);
        let child_descriptor = SourceDescriptor::bundle_member(
            container,
            SourceKind::SerializedFile,
            SourceMemberId::new("nested.assets").unwrap(),
        )
        .unwrap();
        let reversed = [
            PhysicalDomainAddition::new(
                child_descriptor,
                fingerprint(SourceKind::SerializedFile, b"nested assets"),
            ),
            PhysicalDomainAddition::new(
                container_descriptor,
                fingerprint(SourceKind::AssetBundle, b"nested bundle"),
            ),
        ];
        let mut begin_budget = AssetLoadBudget::default();
        let mut reversed_transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let reversed_revision = reversed_transaction.candidate.revision().unwrap();
        let mut reversed_budget = AssetLoadBudget::default();
        assert!(matches!(
            reversed_transaction.rewrite_physical_domain(
                PhysicalDomainRewrite::new(root, &observed, &[], &reversed),
                &mut reversed_budget,
            ),
            Err(CatalogError::PhysicalDomainAdditionParentNotReady { parent })
                if parent == container
        ));
        assert_eq!(reversed_budget.usage(), Default::default());
        assert_eq!(
            reversed_transaction.candidate.revision().unwrap(),
            reversed_revision
        );

        let wrong_kind = [PhysicalDomainAddition::new(
            SourceDescriptor::sidecar(root, SourceMemberId::new("wrong.resS").unwrap()).unwrap(),
            fingerprint(SourceKind::Yaml, b"wrong kind"),
        )];
        let mut begin_budget = AssetLoadBudget::default();
        let mut wrong_kind_transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let wrong_kind_revision = wrong_kind_transaction.candidate.revision().unwrap();
        let mut wrong_kind_budget = budget_with(1, 1);
        assert!(matches!(
            wrong_kind_transaction.rewrite_physical_domain(
                PhysicalDomainRewrite::new(root, &observed, &[], &wrong_kind),
                &mut wrong_kind_budget,
            ),
            Err(CatalogError::SourceKindMismatch {
                expected: SourceKind::StreamedResource,
                actual: SourceKind::Yaml,
            })
        ));
        assert_eq!(wrong_kind_budget.usage(), Default::default());
        assert_eq!(
            wrong_kind_transaction.candidate.revision().unwrap(),
            wrong_kind_revision
        );
    }

    #[test]
    fn revision_fingerprint_lookup_matches_an_equivalent_domain_rewrite() {
        let (catalog, root, webfile, serialized_file, _) = physical_domain_fixture();
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

        let mut observation_budget = AssetLoadBudget::default();
        let observed = catalog
            .physical_domain_sources(root, &mut observation_budget)
            .unwrap();
        let mut changed = vec![
            PhysicalDomainSource::new(
                webfile,
                fingerprint(SourceKind::WebFile, b"changed webfile"),
            ),
            PhysicalDomainSource::new(
                serialized_file,
                fingerprint(SourceKind::SerializedFile, b"changed assets"),
            ),
        ];
        changed.sort_unstable_by_key(PhysicalDomainSource::source);
        let predicted = catalog
            .revision_with_fingerprint_lookup(|source| {
                changed
                    .iter()
                    .find(|change| change.source() == source)
                    .map(PhysicalDomainSource::fingerprint)
            })
            .unwrap();
        let rewrite = PhysicalDomainRewrite::new(root, &observed, &changed, &[]);
        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        transaction
            .rewrite_physical_domain(rewrite, &mut AssetLoadBudget::default())
            .unwrap();
        let candidate = transaction.commit(&mut AssetLoadBudget::default()).unwrap();
        assert_eq!(candidate.revision().unwrap(), predicted);
    }

    #[test]
    fn physical_domain_rewrite_rejects_stale_or_incomplete_observations_without_charging() {
        let (catalog, root, _, _, companion) = physical_domain_fixture();
        let mut observation_budget = AssetLoadBudget::default();
        let observed = catalog
            .physical_domain_sources(root, &mut observation_budget)
            .unwrap();

        let mut incomplete = observed.clone();
        let missing = incomplete.pop().unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut missing_transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut missing_budget = AssetLoadBudget::default();
        assert!(matches!(
            missing_transaction.rewrite_physical_domain(
                PhysicalDomainRewrite::new(root, &incomplete, &[], &[]),
                &mut missing_budget,
            ),
            Err(CatalogError::PhysicalDomainObservationMissing {
                owner,
                source_id,
            }) if owner == root && source_id == missing.source()
        ));
        assert_eq!(missing_budget.usage().entries, 0);
        assert_eq!(missing_budget.usage().bytes, 0);
        assert!(matches!(
            missing_transaction.commit(&mut missing_budget),
            Err(CatalogError::TransactionAborted)
        ));

        let mut stale = observed.clone();
        stale[0] = PhysicalDomainSource::new(
            stale[0].source(),
            fingerprint(stale[0].source().kind(), b"stale observation"),
        );
        let mut begin_budget = AssetLoadBudget::default();
        let mut stale_transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut stale_budget = AssetLoadBudget::default();
        assert!(matches!(
            stale_transaction.rewrite_physical_domain(
                PhysicalDomainRewrite::new(root, &stale, &[], &[]),
                &mut stale_budget,
            ),
            Err(CatalogError::PhysicalDomainFingerprintMismatch { source_id, .. })
                if source_id == stale[0].source()
        ));
        assert_eq!(stale_budget.usage().entries, 0);
        assert_eq!(stale_budget.usage().bytes, 0);
        assert!(matches!(
            stale_transaction.commit(&mut stale_budget),
            Err(CatalogError::TransactionAborted)
        ));

        let mut reversed_changes = observed[..2].to_vec();
        reversed_changes.reverse();
        let mut begin_budget = AssetLoadBudget::default();
        let mut ordering_transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut ordering_budget = AssetLoadBudget::default();
        assert!(matches!(
            ordering_transaction.rewrite_physical_domain(
                PhysicalDomainRewrite::new(root, &observed, &reversed_changes, &[]),
                &mut ordering_budget,
            ),
            Err(CatalogError::PhysicalDomainSourcesNotStrictlyOrdered {
                collection: "changed",
                ..
            })
        ));
        assert_eq!(ordering_budget.usage().entries, 0);
        assert_eq!(ordering_budget.usage().bytes, 0);

        let mut unexpected = observed.clone();
        unexpected.push(PhysicalDomainSource::new(
            companion,
            catalog.fingerprint(companion).unwrap(),
        ));
        unexpected.sort_unstable_by_key(PhysicalDomainSource::source);
        let mut begin_budget = AssetLoadBudget::default();
        let mut unexpected_transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut unexpected_budget = AssetLoadBudget::default();
        assert!(matches!(
            unexpected_transaction.rewrite_physical_domain(
                PhysicalDomainRewrite::new(root, &unexpected, &[], &[]),
                &mut unexpected_budget,
            ),
            Err(CatalogError::PhysicalDomainObservationUnexpected {
                owner,
                source_id,
            }) if owner == root && source_id == companion
        ));
        assert_eq!(unexpected_budget.usage().entries, 0);
        assert_eq!(unexpected_budget.usage().bytes, 0);
    }

    #[test]
    fn physical_domain_rewrite_rejects_cross_domain_changes_and_invalid_additions() {
        let (mut catalog, root, _, serialized_file, companion) = physical_domain_fixture();
        let other_root = catalog
            .register(
                root_descriptor(SourceKind::Archive, "other.apk", b"other"),
                fingerprint(SourceKind::Archive, b"other"),
            )
            .unwrap();
        let existing_descriptor =
            SourceDescriptor::sidecar(root, SourceMemberId::new("existing.resS").unwrap()).unwrap();
        let existing_fingerprint = fingerprint(SourceKind::StreamedResource, b"existing");
        let existing = catalog
            .register(existing_descriptor.clone(), existing_fingerprint)
            .unwrap();
        let mut observation_budget = AssetLoadBudget::default();
        let observed = catalog
            .physical_domain_sources(root, &mut observation_budget)
            .unwrap();

        let outside_change = [PhysicalDomainSource::new(
            companion,
            fingerprint(SourceKind::StreamedResource, b"changed companion"),
        )];
        let mut begin_budget = AssetLoadBudget::default();
        let mut change_transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut change_budget = AssetLoadBudget::default();
        assert!(matches!(
            change_transaction.rewrite_physical_domain(
                PhysicalDomainRewrite::new(root, &observed, &outside_change, &[]),
                &mut change_budget,
            ),
            Err(CatalogError::PhysicalDomainChangeOutsideDomain {
                source_id,
                physical_owner,
            }) if source_id == companion && physical_owner == companion
        ));
        assert!(matches!(
            change_transaction.commit(&mut change_budget),
            Err(CatalogError::TransactionAborted)
        ));

        let companion_addition = [PhysicalDomainAddition::new(
            SourceDescriptor::companion(
                serialized_file,
                SourceMemberId::new("other.resS").unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::StreamedResource, b"other companion"),
        )];
        let mut begin_budget = AssetLoadBudget::default();
        let mut companion_transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut companion_budget = AssetLoadBudget::default();
        assert!(matches!(
            companion_transaction.rewrite_physical_domain(
                PhysicalDomainRewrite::new(root, &observed, &[], &companion_addition),
                &mut companion_budget,
            ),
            Err(CatalogError::PhysicalDomainAdditionRequiresMember {
                location: SourceLocationKind::Companion,
            })
        ));
        assert_eq!(companion_budget.usage().entries, 0);
        assert_eq!(companion_budget.usage().bytes, 0);

        let outside_addition = [PhysicalDomainAddition::new(
            SourceDescriptor::sidecar(other_root, SourceMemberId::new("outside.resS").unwrap())
                .unwrap(),
            fingerprint(SourceKind::StreamedResource, b"outside"),
        )];
        let mut begin_budget = AssetLoadBudget::default();
        let mut outside_transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut outside_budget = AssetLoadBudget::default();
        assert!(matches!(
            outside_transaction.rewrite_physical_domain(
                PhysicalDomainRewrite::new(root, &observed, &[], &outside_addition),
                &mut outside_budget,
            ),
            Err(CatalogError::PhysicalDomainAdditionOutsideDomain {
                parent,
                physical_owner,
            }) if parent == other_root && physical_owner == other_root
        ));
        assert_eq!(outside_budget.usage().entries, 0);
        assert_eq!(outside_budget.usage().bytes, 0);

        let existing_addition = [PhysicalDomainAddition::new(
            existing_descriptor,
            existing_fingerprint,
        )];
        let mut begin_budget = AssetLoadBudget::default();
        let mut existing_transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut existing_budget = AssetLoadBudget::default();
        assert!(matches!(
            existing_transaction.rewrite_physical_domain(
                PhysicalDomainRewrite::new(root, &observed, &[], &existing_addition),
                &mut existing_budget,
            ),
            Err(CatalogError::PhysicalDomainAdditionAlreadyExists { source_id })
                if source_id == existing
        ));
        assert_eq!(existing_budget.usage().entries, 0);
        assert_eq!(existing_budget.usage().bytes, 0);

        let duplicate_descriptor =
            SourceDescriptor::sidecar(root, SourceMemberId::new("duplicate.resS").unwrap())
                .unwrap();
        let duplicate_fingerprint = fingerprint(SourceKind::StreamedResource, b"duplicate");
        let duplicate_additions = [
            PhysicalDomainAddition::new(duplicate_descriptor.clone(), duplicate_fingerprint),
            PhysicalDomainAddition::new(duplicate_descriptor, duplicate_fingerprint),
        ];
        let mut begin_budget = AssetLoadBudget::default();
        let mut duplicate_transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let candidate_revision = duplicate_transaction.candidate.revision().unwrap();
        let mut duplicate_budget = AssetLoadBudget::default();
        assert!(matches!(
            duplicate_transaction.rewrite_physical_domain(
                PhysicalDomainRewrite::new(root, &observed, &[], &duplicate_additions),
                &mut duplicate_budget,
            ),
            Err(CatalogError::PhysicalDomainAdditionAlreadyExists { .. })
        ));
        assert_eq!(duplicate_budget.usage().entries, 0);
        assert_eq!(duplicate_budget.usage().bytes, 0);
        assert_eq!(
            duplicate_transaction.candidate.revision().unwrap(),
            candidate_revision
        );
        assert!(matches!(
            duplicate_transaction.commit(&mut duplicate_budget),
            Err(CatalogError::TransactionAborted)
        ));
    }

    #[test]
    fn physical_domain_rewrite_budget_failure_poisoned_transaction_before_mutation() {
        let (catalog, root, _, serialized_file, companion) = physical_domain_fixture();
        let original_revision = catalog.revision().unwrap();
        let mut observation_budget = AssetLoadBudget::default();
        let observed = catalog
            .physical_domain_sources(root, &mut observation_budget)
            .unwrap();
        let changes = [PhysicalDomainSource::new(
            serialized_file,
            fingerprint(SourceKind::SerializedFile, b"changed asset"),
        )];

        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let planned = checked_record_clone_bytes(
            &transaction
                .candidate
                .by_id
                .get(&serialized_file)
                .unwrap()
                .descriptor,
        )
        .unwrap();
        let mut tiny_budget = budget_with(planned - 1, 1);
        assert!(matches!(
            transaction.rewrite_physical_domain(
                PhysicalDomainRewrite::new(root, &observed, &changes, &[]),
                &mut tiny_budget,
            ),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(tiny_budget.usage().entries, 0);
        assert_eq!(tiny_budget.usage().bytes, 0);
        assert!(matches!(
            transaction.commit(&mut tiny_budget),
            Err(CatalogError::TransactionAborted)
        ));
        assert_eq!(catalog.revision().unwrap(), original_revision);
        assert_eq!(
            catalog.fingerprint(serialized_file).unwrap(),
            fingerprint(SourceKind::SerializedFile, b"asset")
        );
        assert_eq!(
            catalog.fingerprint(companion).unwrap(),
            fingerprint(SourceKind::StreamedResource, b"companion")
        );

        let addition = PhysicalDomainAddition::new(
            SourceDescriptor::sidecar(root, SourceMemberId::new("budget.resS").unwrap()).unwrap(),
            fingerprint(SourceKind::StreamedResource, b"resource"),
        );
        let additions = [addition];
        let mut begin_budget = AssetLoadBudget::default();
        let mut addition_transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let rewrite = PhysicalDomainRewrite::new(root, &observed, &[], &additions);
        let planned = addition_transaction
            .candidate
            .checked_physical_domain_rewrite_bytes(rewrite)
            .unwrap();
        let capacities_before = catalog_index_capacities(&addition_transaction.candidate);
        let mut tiny_addition_budget = budget_with(planned - 1, 1);
        assert!(matches!(
            addition_transaction.rewrite_physical_domain(rewrite, &mut tiny_addition_budget),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(tiny_addition_budget.usage().entries, 0);
        assert_eq!(tiny_addition_budget.usage().bytes, 0);
        assert_eq!(
            catalog_index_capacities(&addition_transaction.candidate),
            capacities_before
        );
        assert!(matches!(
            addition_transaction.commit(&mut tiny_addition_budget),
            Err(CatalogError::TransactionAborted)
        ));
        assert_eq!(catalog.revision().unwrap(), original_revision);
    }

    #[test]
    fn revision_includes_root_physical_binding_without_changing_logical_identity() {
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
        assert_ne!(first.revision().unwrap(), second.revision().unwrap());
    }

    #[test]
    fn verified_binding_construction_is_exact_budgeted_and_rejects_wrong_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.assets");
        let contents = b"verified asset bytes";
        fs::write(&path, contents).unwrap();
        let expected = fingerprint(SourceKind::SerializedFile, contents);
        let origin = PhysicalOrigin::from_existing_path(&path).unwrap();
        let planned = checked_byte_add(
            contents.len() as u64,
            origin.path().as_os_str().len() as u64,
        )
        .unwrap();

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
            binding.revalidate_current_contents(),
            Err(CatalogError::VerifiedPhysicalBindingChanged { .. })
        ));
    }

    #[test]
    fn verified_binding_revalidation_is_exact_budgeted_even_for_noop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.assets");
        let contents = b"stable asset bytes";
        fs::write(&path, contents).unwrap();
        let origin = PhysicalOrigin::from_existing_path(&path).unwrap();
        let source_fingerprint = fingerprint(SourceKind::SerializedFile, contents);
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let source = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("main.assets").unwrap(),
                    origin.clone(),
                ),
                source_fingerprint,
            )
            .unwrap();
        let original_revision = catalog.revision().unwrap();

        let mut verification_budget = AssetLoadBudget::default();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &path,
            source_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut rejected = catalog.begin_transaction(&mut begin_budget).unwrap();
        let apply_bytes = rejected
            .checked_verified_binding_apply_bytes(source, &binding)
            .unwrap();
        let planned = checked_byte_add(apply_bytes, binding.revalidation_bytes()).unwrap();
        assert!(planned >= contents.len() as u64 * 2);
        let mut one_short = budget_with(planned - 1, 1);
        rejected
            .replace_verified_binding(source, binding, &mut one_short)
            .unwrap();
        assert_eq!(one_short.usage().bytes, apply_bytes);
        assert!(matches!(
            rejected.commit(&mut one_short),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(one_short.usage().bytes, apply_bytes);
        assert_eq!(catalog.revision().unwrap(), original_revision);

        let mut verification_budget = AssetLoadBudget::default();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &path,
            source_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut exact = budget_with(planned, 1);
        transaction
            .replace_verified_binding(source, binding, &mut exact)
            .unwrap();
        let candidate = transaction.commit(&mut exact).unwrap();

        assert_eq!(exact.usage().bytes, planned);
        assert_eq!(candidate.revision().unwrap(), original_revision);
        assert_eq!(candidate.physical_origin(source).unwrap(), &origin);
        assert_eq!(candidate.find_physical(&origin), Some(source));
    }

    #[test]
    fn domain_rewrite_cannot_silently_supersede_a_pending_physical_proof() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.assets");
        let contents = b"verified base asset";
        fs::write(&path, contents).unwrap();
        let source_fingerprint = fingerprint(SourceKind::SerializedFile, contents);
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let source = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("main.assets").unwrap(),
                    PhysicalOrigin::from_existing_path(&path).unwrap(),
                ),
                source_fingerprint,
            )
            .unwrap();
        let mut observation_budget = AssetLoadBudget::default();
        let observed = catalog
            .physical_domain_sources(source, &mut observation_budget)
            .unwrap();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &path,
            source_fingerprint,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let mut transaction = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        transaction
            .replace_verified_binding(source, binding, &mut operation_budget)
            .unwrap();
        let usage_before_rewrite = operation_budget.usage();
        let revision_before_rewrite = transaction.candidate.revision().unwrap();
        let replacement = fingerprint(SourceKind::SerializedFile, b"prepared output");
        let changed = [PhysicalDomainSource::new(source, replacement)];

        assert!(matches!(
            transaction.rewrite_physical_domain(
                PhysicalDomainRewrite::new(source, &observed, &changed, &[]),
                &mut operation_budget,
            ),
            Err(CatalogError::PendingPhysicalVerificationSuperseded {
                source_id,
                verified,
                replacement: actual_replacement,
            }) if source_id == source
                && verified == source_fingerprint
                && actual_replacement == replacement
        ));
        assert_eq!(operation_budget.usage(), usage_before_rewrite);
        assert_eq!(
            transaction.candidate.revision().unwrap(),
            revision_before_rewrite
        );
        assert!(matches!(
            transaction.commit(&mut operation_budget),
            Err(CatalogError::TransactionAborted)
        ));
    }

    #[test]
    fn fingerprint_rewrite_cannot_silently_discard_a_pending_physical_proof() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.assets");
        let contents = b"verified base asset";
        fs::write(&path, contents).unwrap();
        let source_fingerprint = fingerprint(SourceKind::SerializedFile, contents);
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let source = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("main.assets").unwrap(),
                    PhysicalOrigin::from_existing_path(&path).unwrap(),
                ),
                source_fingerprint,
            )
            .unwrap();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &path,
            source_fingerprint,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let mut transaction = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        transaction
            .replace_verified_binding(source, binding, &mut operation_budget)
            .unwrap();
        let usage_before_rewrite = operation_budget.usage();
        let revision_before_rewrite = transaction.candidate.revision().unwrap();
        let replacement = fingerprint(SourceKind::SerializedFile, b"prepared output");

        assert!(matches!(
            transaction.replace_fingerprint(source, replacement, &mut operation_budget),
            Err(CatalogError::PendingPhysicalVerificationSuperseded {
                source_id,
                verified,
                replacement: actual_replacement,
            }) if source_id == source
                && verified == source_fingerprint
                && actual_replacement == replacement
        ));
        assert_eq!(operation_budget.usage(), usage_before_rewrite);
        assert_eq!(
            transaction.candidate.revision().unwrap(),
            revision_before_rewrite
        );
        assert!(matches!(
            transaction.commit(&mut operation_budget),
            Err(CatalogError::TransactionAborted)
        ));
    }

    #[test]
    fn verified_binding_commit_charges_only_the_final_superseding_proof() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.assets");
        let contents = b"stable asset bytes";
        fs::write(&path, contents).unwrap();
        let origin = PhysicalOrigin::from_existing_path(&path).unwrap();
        let source_fingerprint = fingerprint(SourceKind::SerializedFile, contents);
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let source = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("main.assets").unwrap(),
                    origin,
                ),
                source_fingerprint,
            )
            .unwrap();
        let mut first_verification_budget = AssetLoadBudget::default();
        let first = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &path,
            source_fingerprint,
            &mut first_verification_budget,
        )
        .unwrap();
        let mut second_verification_budget = AssetLoadBudget::default();
        let second = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &path,
            source_fingerprint,
            &mut second_verification_budget,
        )
        .unwrap();

        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        transaction
            .replace_verified_binding(source, first, &mut operation_budget)
            .unwrap();
        let after_first_apply = operation_budget.usage().bytes;
        transaction
            .replace_verified_binding(source, second, &mut operation_budget)
            .unwrap();
        let after_second_apply = operation_budget.usage().bytes;
        assert_eq!(
            after_second_apply - after_first_apply,
            contents.len() as u64
        );

        let candidate = transaction.commit(&mut operation_budget).unwrap();
        assert_eq!(
            operation_budget.usage().bytes - after_second_apply,
            contents.len() as u64
        );
        assert_eq!(candidate.fingerprint(source).unwrap(), source_fingerprint);
    }

    #[test]
    fn removed_or_aborted_proofs_do_not_charge_future_commit_work() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.assets");
        let contents = b"stable asset bytes";
        fs::write(&path, contents).unwrap();
        let origin = PhysicalOrigin::from_existing_path(&path).unwrap();
        let source_fingerprint = fingerprint(SourceKind::SerializedFile, contents);
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let source = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("main.assets").unwrap(),
                    origin,
                ),
                source_fingerprint,
            )
            .unwrap();

        let mut verification_budget = AssetLoadBudget::default();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &path,
            source_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut removed = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        removed
            .replace_verified_binding(source, binding, &mut operation_budget)
            .unwrap();
        removed
            .remove_subtree(source, &mut operation_budget)
            .unwrap();
        let before_removed_commit = operation_budget.usage().bytes;
        let candidate = removed.commit(&mut operation_budget).unwrap();
        assert_eq!(operation_budget.usage().bytes, before_removed_commit);
        assert!(candidate.is_empty());

        let mut verification_budget = AssetLoadBudget::default();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &path,
            source_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut aborted = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        aborted
            .replace_verified_binding(source, binding, &mut operation_budget)
            .unwrap();
        assert!(matches!(
            aborted.replace_fingerprint(
                source,
                fingerprint(SourceKind::Yaml, b"wrong kind"),
                &mut operation_budget,
            ),
            Err(CatalogError::SourceKindMismatch { .. })
        ));
        let before_aborted_commit = operation_budget.usage().bytes;
        assert!(matches!(
            aborted.commit(&mut operation_budget),
            Err(CatalogError::TransactionAborted)
        ));
        assert_eq!(operation_budget.usage().bytes, before_aborted_commit);
    }

    #[test]
    fn verified_binding_commit_rejects_contents_changed_after_apply() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.assets");
        fs::write(&path, b"before").unwrap();
        let origin = PhysicalOrigin::from_existing_path(&path).unwrap();
        let source_fingerprint = fingerprint(SourceKind::SerializedFile, b"before");
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let source = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("main.assets").unwrap(),
                    origin,
                ),
                source_fingerprint,
            )
            .unwrap();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &path,
            source_fingerprint,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let mut transaction = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        transaction
            .replace_verified_binding(source, binding, &mut AssetLoadBudget::default())
            .unwrap();

        fs::write(&path, b"change").unwrap();
        let mut commit_budget = AssetLoadBudget::default();
        assert!(matches!(
            transaction.commit(&mut commit_budget),
            Err(CatalogError::VerifiedPhysicalBindingChanged { .. }
                | CatalogError::VerifiedFingerprintMismatch { .. })
        ));
        assert_eq!(commit_budget.usage().bytes, 0);
    }

    #[test]
    fn verified_binding_commit_rejects_byte_identical_path_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.assets");
        let replacement = directory.path().join("replacement.assets");
        fs::write(&path, b"same bytes").unwrap();
        fs::write(&replacement, b"same bytes").unwrap();
        let origin = PhysicalOrigin::from_existing_path(&path).unwrap();
        let source_fingerprint = fingerprint(SourceKind::SerializedFile, b"same bytes");
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let source = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("main.assets").unwrap(),
                    origin,
                ),
                source_fingerprint,
            )
            .unwrap();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &path,
            source_fingerprint,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let mut transaction = catalog
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        transaction
            .replace_verified_binding(source, binding, &mut AssetLoadBudget::default())
            .unwrap();

        fs::remove_file(&path).unwrap();
        fs::rename(&replacement, &path).unwrap();
        let mut commit_budget = AssetLoadBudget::default();
        assert!(matches!(
            transaction.commit(&mut commit_budget),
            Err(CatalogError::VerifiedPhysicalBindingChanged { .. })
        ));
        assert_eq!(commit_budget.usage().bytes, 0);
    }

    #[test]
    fn commit_preflights_all_proofs_and_never_charges_partial_revalidation() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.assets");
        let second_path = directory.path().join("second.assets");
        let first_contents = b"first";
        let second_contents = b"second";
        fs::write(&first_path, first_contents).unwrap();
        fs::write(&second_path, second_contents).unwrap();
        let first_fingerprint = fingerprint(SourceKind::SerializedFile, first_contents);
        let second_fingerprint = fingerprint(SourceKind::SerializedFile, second_contents);
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let first_source = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("first.assets").unwrap(),
                    PhysicalOrigin::from_existing_path(&first_path).unwrap(),
                ),
                first_fingerprint,
            )
            .unwrap();
        let second_source = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("second.assets").unwrap(),
                    PhysicalOrigin::from_existing_path(&second_path).unwrap(),
                ),
                second_fingerprint,
            )
            .unwrap();

        let verify = |source_path: &Path, source_fingerprint| {
            VerifiedPhysicalBinding::verify_existing(
                SourceKind::SerializedFile,
                source_path,
                source_fingerprint,
                &mut AssetLoadBudget::default(),
            )
            .unwrap()
        };
        let mut begin_budget = AssetLoadBudget::default();
        let mut preflight = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        preflight
            .replace_verified_binding(
                first_source,
                verify(&first_path, first_fingerprint),
                &mut operation_budget,
            )
            .unwrap();
        preflight
            .replace_verified_binding(
                second_source,
                verify(&second_path, second_fingerprint),
                &mut operation_budget,
            )
            .unwrap();
        fs::remove_file(&first_path).unwrap();
        let revalidation_bytes =
            checked_usize_to_u64(first_contents.len() + second_contents.len()).unwrap();
        let mut one_short = budget_with(revalidation_bytes - 1, 1);
        assert!(matches!(
            preflight.commit(&mut one_short),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(one_short.usage().bytes, 0);

        fs::write(&first_path, first_contents).unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut partial_failure = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        partial_failure
            .replace_verified_binding(
                first_source,
                verify(&first_path, first_fingerprint),
                &mut operation_budget,
            )
            .unwrap();
        partial_failure
            .replace_verified_binding(
                second_source,
                verify(&second_path, second_fingerprint),
                &mut operation_budget,
            )
            .unwrap();
        fs::write(&second_path, b"changed second").unwrap();
        let mut commit_budget = AssetLoadBudget::default();
        assert!(matches!(
            partial_failure.commit(&mut commit_budget),
            Err(CatalogError::VerifiedPhysicalBindingChanged { .. }
                | CatalogError::VerifiedFingerprintMismatch { .. })
        ));
        assert_eq!(commit_budget.usage().bytes, 0);
    }

    #[test]
    fn verified_binding_revalidation_rejects_same_length_in_place_rewrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.assets");
        let original_contents = b"AAAA";
        let changed_contents = b"BBBB";
        fs::write(&path, original_contents).unwrap();
        let original_modified = fs::metadata(&path).unwrap().modified().unwrap();
        let origin = PhysicalOrigin::from_existing_path(&path).unwrap();
        let original_fingerprint = fingerprint(SourceKind::SerializedFile, original_contents);
        let changed_fingerprint = fingerprint(SourceKind::SerializedFile, changed_contents);
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let source = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("main.assets").unwrap(),
                    origin.clone(),
                ),
                original_fingerprint,
            )
            .unwrap();
        let original_revision = catalog.revision().unwrap();

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

        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        assert!(matches!(
            transaction.replace_verified_binding(source, binding, &mut operation_budget),
            Err(CatalogError::VerifiedFingerprintMismatch { expected, actual })
                if expected == original_fingerprint && actual == changed_fingerprint
        ));
        assert_eq!(operation_budget.usage().bytes, 0);
        assert!(matches!(
            transaction.commit(&mut operation_budget),
            Err(CatalogError::TransactionAborted)
        ));

        assert_eq!(catalog.revision().unwrap(), original_revision);
        assert_eq!(catalog.physical_origin(source).unwrap(), &origin);
        assert_eq!(catalog.find_physical(&origin), Some(source));
        assert_eq!(catalog.fingerprint(source).unwrap(), original_fingerprint);
    }

    #[test]
    fn companion_identity_is_stable_across_unbound_and_verified_bindings() {
        let directory = tempfile::tempdir().unwrap();
        let first_companion_path = directory.path().join("main.resS");
        let second_companion_path = directory.path().join("main.resource");
        fs::write(&first_companion_path, b"resource").unwrap();
        fs::write(&second_companion_path, b"resource").unwrap();
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

        let mut verification_budget = AssetLoadBudget::default();
        let first_binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::StreamedResource,
            &first_companion_path,
            source_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        let mut transaction = first
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        transaction
            .replace_verified_binding(first_companion, first_binding, &mut operation_budget)
            .unwrap();
        first = transaction.commit(&mut operation_budget).unwrap();

        let mut verification_budget = AssetLoadBudget::default();
        let second_binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::StreamedResource,
            &second_companion_path,
            source_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        let mut transaction = second
            .begin_transaction(&mut AssetLoadBudget::default())
            .unwrap();
        transaction
            .replace_verified_binding(second_companion, second_binding, &mut operation_budget)
            .unwrap();
        second = transaction.commit(&mut operation_budget).unwrap();

        let first_companion_origin =
            PhysicalOrigin::from_existing_path(&first_companion_path).unwrap();
        let second_companion_origin =
            PhysicalOrigin::from_existing_path(&second_companion_path).unwrap();
        assert_eq!(
            first.physical_origin(first_companion).unwrap(),
            &first_companion_origin
        );
        assert_eq!(
            second.physical_origin(second_companion).unwrap(),
            &second_companion_origin
        );
        assert_eq!(
            first.find_physical(&first_companion_origin),
            Some(first_companion)
        );
        assert_eq!(
            first.physical_origin_option(first_companion).unwrap(),
            Some(&first_companion_origin)
        );
        assert_ne!(first.revision().unwrap(), unbound_revision);
        assert_ne!(first.revision().unwrap(), second.revision().unwrap());
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
    fn fingerprint_replacement_preserves_source_identity_and_is_budget_atomic() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let source = catalog
            .register(
                root_descriptor(SourceKind::SerializedFile, "main.assets", b"old"),
                fingerprint(SourceKind::SerializedFile, b"old"),
            )
            .unwrap();
        let original_revision = catalog.revision().unwrap();
        let original_locator = catalog.source_locator(source).unwrap().clone();
        let original_origin = catalog.physical_origin(source).unwrap().clone();

        let mut begin_budget = AssetLoadBudget::default();
        let mut rejected = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut tiny_budget = budget_with(1, 1);
        assert!(matches!(
            rejected.replace_fingerprint(
                source,
                fingerprint(SourceKind::SerializedFile, b"new"),
                &mut tiny_budget,
            ),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(tiny_budget.usage().bytes, 0);
        assert!(matches!(
            rejected.commit(&mut tiny_budget),
            Err(CatalogError::TransactionAborted)
        ));
        assert_eq!(catalog.revision().unwrap(), original_revision);

        let mut budget = AssetLoadBudget::default();
        let mut wrong_kind = catalog.begin_transaction(&mut budget).unwrap();
        assert!(matches!(
            wrong_kind.replace_fingerprint(
                source,
                fingerprint(SourceKind::Yaml, b"new"),
                &mut budget,
            ),
            Err(CatalogError::SourceKindMismatch {
                expected: SourceKind::SerializedFile,
                actual: SourceKind::Yaml,
            })
        ));
        assert!(matches!(
            wrong_kind.commit(&mut budget),
            Err(CatalogError::TransactionAborted)
        ));
        assert_eq!(catalog.revision().unwrap(), original_revision);

        let mut budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut budget).unwrap();
        transaction
            .replace_fingerprint(
                source,
                fingerprint(SourceKind::SerializedFile, b"new"),
                &mut budget,
            )
            .unwrap();
        let candidate = transaction.commit(&mut budget).unwrap();

        assert!(candidate.contains(source));
        assert_eq!(candidate.source_locator(source).unwrap(), &original_locator);
        assert_eq!(candidate.physical_origin(source).unwrap(), &original_origin);
        assert_eq!(
            candidate.fingerprint(source).unwrap(),
            fingerprint(SourceKind::SerializedFile, b"new")
        );
        assert_ne!(candidate.revision().unwrap(), original_revision);
        candidate.validate().unwrap();
    }

    #[test]
    fn verified_root_binding_is_atomic_and_updates_only_inherited_members() {
        let directory = tempfile::tempdir().unwrap();
        let old_root_path = directory.path().join("old.apk");
        let new_root_path = directory.path().join("new.apk");
        let companion_path = directory.path().join("main.resS");
        fs::write(&old_root_path, b"archive").unwrap();
        fs::write(&new_root_path, b"archive").unwrap();
        fs::write(&companion_path, b"resource").unwrap();
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let old_root_origin = PhysicalOrigin::from_existing_path(&old_root_path).unwrap();
        let new_root_origin = PhysicalOrigin::from_existing_path(&new_root_path).unwrap();
        let companion_origin = PhysicalOrigin::from_existing_path(&companion_path).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let root = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::Archive,
                    SourceAlias::new("game.apk").unwrap(),
                    old_root_origin.clone(),
                ),
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
        let mut budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut budget).unwrap();
        let companion = transaction
            .register_companion(
                member,
                SourceMemberId::new("main.resS").unwrap(),
                fingerprint(SourceKind::StreamedResource, b"resource"),
                &mut budget,
            )
            .unwrap();
        catalog = transaction.commit(&mut budget).unwrap();

        let mut verification_budget = AssetLoadBudget::default();
        let companion_binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::StreamedResource,
            &companion_path,
            fingerprint(SourceKind::StreamedResource, b"resource"),
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        transaction
            .replace_verified_binding(companion, companion_binding, &mut operation_budget)
            .unwrap();
        catalog = transaction.commit(&mut operation_budget).unwrap();

        let original_revision = catalog.revision().unwrap();
        let root_locator = catalog.source_locator(root).unwrap().clone();
        let member_locator = catalog.source_locator(member).unwrap().clone();
        let companion_locator = catalog.source_locator(companion).unwrap().clone();

        let mut verification_budget = AssetLoadBudget::default();
        let conflicting_binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::Archive,
            &companion_path,
            fingerprint(SourceKind::Archive, b"resource"),
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut conflict = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        assert!(matches!(
            conflict.replace_verified_binding(root, conflicting_binding, &mut operation_budget),
            Err(CatalogError::SubtreeReplacementRequired { source_id }) if source_id == root
        ));
        assert_eq!(operation_budget.usage().bytes, 0);
        assert!(matches!(
            conflict.commit(&mut operation_budget),
            Err(CatalogError::TransactionAborted)
        ));
        assert_eq!(catalog.revision().unwrap(), original_revision);

        let new_fingerprint = fingerprint(SourceKind::Archive, b"archive");
        let mut verification_budget = AssetLoadBudget::default();
        let rejected_binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::Archive,
            &new_root_path,
            new_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut rejected = catalog.begin_transaction(&mut begin_budget).unwrap();
        let apply_bytes = rejected
            .checked_verified_binding_apply_bytes(root, &rejected_binding)
            .unwrap();
        let planned = checked_byte_add(apply_bytes, rejected_binding.revalidation_bytes()).unwrap();
        let mut tiny_budget = budget_with(apply_bytes - 1, 1);
        assert!(matches!(
            rejected.replace_verified_binding(root, rejected_binding, &mut tiny_budget),
            Err(CatalogError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(tiny_budget.usage().bytes, 0);
        assert!(matches!(
            rejected.commit(&mut tiny_budget),
            Err(CatalogError::TransactionAborted)
        ));
        assert_eq!(catalog.revision().unwrap(), original_revision);

        let mut verification_budget = AssetLoadBudget::default();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::Archive,
            &new_root_path,
            new_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut exact = budget_with(planned, 1);
        transaction
            .replace_verified_binding(root, binding, &mut exact)
            .unwrap();
        let candidate = transaction.commit(&mut exact).unwrap();
        assert_eq!(exact.usage().bytes, planned);

        assert_eq!(candidate.source_locator(root).unwrap(), &root_locator);
        assert_eq!(candidate.source_locator(member).unwrap(), &member_locator);
        assert_eq!(
            candidate.source_locator(companion).unwrap(),
            &companion_locator
        );
        assert_eq!(candidate.physical_origin(root).unwrap(), &new_root_origin);
        assert_eq!(candidate.physical_origin(member).unwrap(), &new_root_origin);
        assert_eq!(
            candidate.physical_origin(companion).unwrap(),
            &companion_origin
        );
        assert_eq!(candidate.find_physical(&old_root_origin), None);
        assert_eq!(candidate.find_physical(&new_root_origin), Some(root));
        assert_eq!(candidate.find_physical(&companion_origin), Some(companion));
        assert_eq!(candidate.fingerprint(root).unwrap(), new_fingerprint);
        assert_eq!(
            candidate.fingerprint(member).unwrap(),
            fingerprint(SourceKind::SerializedFile, b"asset")
        );
        assert_ne!(candidate.revision().unwrap(), original_revision);
        candidate.validate().unwrap();
    }

    #[test]
    fn root_fingerprint_change_with_members_requires_atomic_subtree_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let old_root_path = directory.path().join("old.apk");
        let new_root_path = directory.path().join("new.apk");
        fs::write(&old_root_path, b"old archive").unwrap();
        fs::write(&new_root_path, b"new archive").unwrap();
        let old_root_origin = PhysicalOrigin::from_existing_path(&old_root_path).unwrap();
        let new_root_origin = PhysicalOrigin::from_existing_path(&new_root_path).unwrap();
        let old_fingerprint = fingerprint(SourceKind::Archive, b"old archive");
        let new_fingerprint = fingerprint(SourceKind::Archive, b"new archive");
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let root = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::Archive,
                    SourceAlias::new("game.apk").unwrap(),
                    old_root_origin.clone(),
                ),
                old_fingerprint,
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
        let original_revision = catalog.revision().unwrap();
        let original_root_locator = catalog.source_locator(root).unwrap().clone();

        let mut verification_budget = AssetLoadBudget::default();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::Archive,
            &new_root_path,
            new_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        assert!(matches!(
            transaction.replace_verified_binding(root, binding, &mut operation_budget),
            Err(CatalogError::SubtreeReplacementRequired { source_id }) if source_id == root
        ));
        assert_eq!(operation_budget.usage().bytes, 0);
        assert_eq!(operation_budget.usage().entries, 0);
        assert!(matches!(
            transaction.commit(&mut operation_budget),
            Err(CatalogError::TransactionAborted)
        ));

        assert_eq!(catalog.revision().unwrap(), original_revision);
        assert_eq!(
            catalog.source_locator(root).unwrap(),
            &original_root_locator
        );
        assert_eq!(catalog.fingerprint(root).unwrap(), old_fingerprint);
        assert_eq!(catalog.physical_origin(root).unwrap(), &old_root_origin);
        assert_eq!(catalog.physical_origin(webfile).unwrap(), &old_root_origin);
        assert_eq!(
            catalog.physical_origin(serialized_file).unwrap(),
            &old_root_origin
        );
        assert_eq!(catalog.find_physical(&old_root_origin), Some(root));
        assert_eq!(catalog.find_physical(&new_root_origin), None);

        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        assert!(matches!(
            transaction.replace_fingerprint(root, new_fingerprint, &mut operation_budget),
            Err(CatalogError::SubtreeReplacementRequired { source_id }) if source_id == root
        ));
        assert_eq!(operation_budget.usage().bytes, 0);
        assert_eq!(operation_budget.usage().entries, 0);
        assert!(matches!(
            transaction.commit(&mut operation_budget),
            Err(CatalogError::TransactionAborted)
        ));
        assert_eq!(catalog.revision().unwrap(), original_revision);
        assert_eq!(catalog.find_physical(&old_root_origin), Some(root));
        assert_eq!(catalog.find_physical(&new_root_origin), None);
    }

    #[test]
    fn nested_member_fingerprint_change_requires_physical_owner_subtree_replacement() {
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
                fingerprint(SourceKind::WebFile, b"old webfile"),
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
        let original_revision = catalog.revision().unwrap();

        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        assert!(matches!(
            transaction.replace_fingerprint(
                webfile,
                fingerprint(SourceKind::WebFile, b"new webfile"),
                &mut operation_budget,
            ),
            Err(CatalogError::InheritedSourceReplacementRequired {
                source_id,
                physical_owner,
            }) if source_id == webfile && physical_owner == root
        ));
        assert_eq!(operation_budget.usage().bytes, 0);
        assert_eq!(operation_budget.usage().entries, 0);
        assert!(matches!(
            transaction.commit(&mut operation_budget),
            Err(CatalogError::TransactionAborted)
        ));

        assert_eq!(catalog.revision().unwrap(), original_revision);
        assert_eq!(
            catalog.fingerprint(webfile).unwrap(),
            fingerprint(SourceKind::WebFile, b"old webfile")
        );
        assert_eq!(
            catalog.fingerprint(serialized_file).unwrap(),
            fingerprint(SourceKind::SerializedFile, b"asset")
        );
        catalog.validate().unwrap();
    }

    #[test]
    fn inherited_leaf_change_aborts_transaction_with_pending_root_proof() {
        let directory = tempfile::tempdir().unwrap();
        let root_path = directory.path().join("game.apk");
        fs::write(&root_path, b"archive").unwrap();
        let root_origin = PhysicalOrigin::from_existing_path(&root_path).unwrap();
        let root_fingerprint = fingerprint(SourceKind::Archive, b"archive");
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let root = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::Archive,
                    SourceAlias::new("game.apk").unwrap(),
                    root_origin,
                ),
                root_fingerprint,
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
        let leaf = catalog
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
        let original_revision = catalog.revision().unwrap();

        let mut verification_budget = AssetLoadBudget::default();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::Archive,
            &root_path,
            root_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut binding_budget = AssetLoadBudget::default();
        transaction
            .replace_verified_binding(root, binding, &mut binding_budget)
            .unwrap();

        let mut replacement_budget = AssetLoadBudget::default();
        assert!(matches!(
            transaction.replace_fingerprint(
                leaf,
                fingerprint(SourceKind::SerializedFile, b"changed"),
                &mut replacement_budget,
            ),
            Err(CatalogError::InheritedSourceReplacementRequired {
                source_id,
                physical_owner,
            }) if source_id == leaf && physical_owner == root
        ));
        assert_eq!(replacement_budget.usage().bytes, 0);
        assert_eq!(replacement_budget.usage().entries, 0);
        assert!(matches!(
            transaction.commit(&mut replacement_budget),
            Err(CatalogError::TransactionAborted)
        ));
        assert_eq!(catalog.revision().unwrap(), original_revision);
        assert_eq!(
            catalog.fingerprint(leaf).unwrap(),
            fingerprint(SourceKind::SerializedFile, b"asset")
        );
        catalog.validate().unwrap();
    }

    #[test]
    fn root_fingerprint_change_with_only_companion_is_allowed_and_not_propagated() {
        let directory = tempfile::tempdir().unwrap();
        let old_root_path = directory.path().join("old.assets");
        let new_root_path = directory.path().join("new.assets");
        fs::write(&old_root_path, b"old asset").unwrap();
        fs::write(&new_root_path, b"new asset").unwrap();
        let old_root_origin = PhysicalOrigin::from_existing_path(&old_root_path).unwrap();
        let new_root_origin = PhysicalOrigin::from_existing_path(&new_root_path).unwrap();
        let old_fingerprint = fingerprint(SourceKind::SerializedFile, b"old asset");
        let new_fingerprint = fingerprint(SourceKind::SerializedFile, b"new asset");
        let companion_fingerprint = fingerprint(SourceKind::StreamedResource, b"resource");
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let root = catalog
            .register(
                SourceDescriptor::root(
                    SourceKind::SerializedFile,
                    SourceAlias::new("main.assets").unwrap(),
                    old_root_origin.clone(),
                ),
                old_fingerprint,
            )
            .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        let companion = transaction
            .register_companion(
                root,
                SourceMemberId::new("main.resS").unwrap(),
                companion_fingerprint,
                &mut operation_budget,
            )
            .unwrap();
        catalog = transaction.commit(&mut operation_budget).unwrap();

        let mut verification_budget = AssetLoadBudget::default();
        let binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &new_root_path,
            new_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        transaction
            .replace_verified_binding(root, binding, &mut operation_budget)
            .unwrap();
        let candidate = transaction.commit(&mut operation_budget).unwrap();

        assert_eq!(candidate.fingerprint(root).unwrap(), new_fingerprint);
        assert_eq!(
            candidate.fingerprint(companion).unwrap(),
            companion_fingerprint
        );
        assert_eq!(candidate.physical_origin(root).unwrap(), &new_root_origin);
        assert_eq!(candidate.physical_origin_option(companion).unwrap(), None);
        assert_eq!(candidate.find_physical(&old_root_origin), None);
        assert_eq!(candidate.find_physical(&new_root_origin), Some(root));
        candidate.validate().unwrap();
    }

    #[test]
    fn companion_operations_reject_typed_conflicts_without_publishing() {
        let directory = tempfile::tempdir().unwrap();
        let companion_path = directory.path().join("main.resS");
        let embedded_path = directory.path().join("embedded.assets");
        fs::write(&companion_path, b"resource").unwrap();
        fs::write(&embedded_path, b"embedded").unwrap();
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let parent = catalog
            .register(
                root_descriptor(SourceKind::SerializedFile, "main.assets", b"asset"),
                fingerprint(SourceKind::SerializedFile, b"asset"),
            )
            .unwrap();
        let archive = catalog
            .register(
                root_descriptor(SourceKind::Archive, "game.apk", b"archive"),
                fingerprint(SourceKind::Archive, b"archive"),
            )
            .unwrap();
        let embedded = catalog
            .register(
                SourceDescriptor::archive_member(
                    archive,
                    SourceKind::SerializedFile,
                    SourceMemberId::new("embedded.assets").unwrap(),
                )
                .unwrap(),
                fingerprint(SourceKind::SerializedFile, b"embedded"),
            )
            .unwrap();
        let member = SourceMemberId::new("main.resS").unwrap();
        let companion_fingerprint = fingerprint(SourceKind::StreamedResource, b"resource");
        let mut budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut budget).unwrap();
        let companion = transaction
            .register_companion(parent, member.clone(), companion_fingerprint, &mut budget)
            .unwrap();
        catalog = transaction.commit(&mut budget).unwrap();
        let revision = catalog.revision().unwrap();

        let mut budget = AssetLoadBudget::default();
        let mut duplicate = catalog.begin_transaction(&mut budget).unwrap();
        assert_eq!(
            duplicate
                .register_companion(parent, member.clone(), companion_fingerprint, &mut budget,)
                .unwrap(),
            companion
        );
        let duplicate = duplicate.commit(&mut budget).unwrap();
        assert_eq!(duplicate.revision().unwrap(), revision);

        let mut budget = AssetLoadBudget::default();
        let mut changed_fingerprint = catalog.begin_transaction(&mut budget).unwrap();
        assert!(matches!(
            changed_fingerprint.register_companion(
                parent,
                member.clone(),
                fingerprint(SourceKind::StreamedResource, b"changed"),
                &mut budget,
            ),
            Err(CatalogError::FingerprintConflict { source_id, .. }) if source_id == companion
        ));

        let mut budget = AssetLoadBudget::default();
        let mut wrong_parent = catalog.begin_transaction(&mut budget).unwrap();
        assert!(matches!(
            wrong_parent.register_companion(
                archive,
                SourceMemberId::new("archive.resS").unwrap(),
                companion_fingerprint,
                &mut budget,
            ),
            Err(CatalogError::InvalidCompanionParentKind { parent, actual })
                if parent == archive && actual == SourceKind::Archive
        ));

        let mut budget = AssetLoadBudget::default();
        let mut wrong_type = catalog.begin_transaction(&mut budget).unwrap();
        assert!(matches!(
            wrong_type.register_companion(
                parent,
                SourceMemberId::new("wrong.resS").unwrap(),
                fingerprint(SourceKind::Yaml, b"resource"),
                &mut budget,
            ),
            Err(CatalogError::SourceKindMismatch {
                expected: SourceKind::StreamedResource,
                actual: SourceKind::Yaml,
            })
        ));

        let mut verification_budget = AssetLoadBudget::default();
        let embedded_binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::SerializedFile,
            &embedded_path,
            fingerprint(SourceKind::SerializedFile, b"embedded"),
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut unsupported_binding = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        assert!(matches!(
            unsupported_binding.replace_verified_binding(
                embedded,
                embedded_binding,
                &mut operation_budget,
            ),
            Err(CatalogError::PhysicalOriginBindingUnsupported {
                source_id,
                location: SourceLocationKind::ArchiveMember,
            }) if source_id == embedded
        ));
        assert!(matches!(
            unsupported_binding.commit(&mut operation_budget),
            Err(CatalogError::TransactionAborted)
        ));

        let mut verification_budget = AssetLoadBudget::default();
        let companion_binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::StreamedResource,
            &companion_path,
            companion_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut companion_binding_transaction =
            catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        companion_binding_transaction
            .replace_verified_binding(companion, companion_binding, &mut operation_budget)
            .unwrap();
        let bound = companion_binding_transaction
            .commit(&mut operation_budget)
            .unwrap();
        let companion_origin = PhysicalOrigin::from_existing_path(&companion_path).unwrap();
        assert_eq!(bound.physical_origin(companion).unwrap(), &companion_origin);
        assert!(bound.contains(companion));
        assert_ne!(bound.revision().unwrap(), revision);
        assert_eq!(catalog.revision().unwrap(), revision);

        let mut begin_budget = AssetLoadBudget::default();
        let mut second_registration = bound.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        let second = second_registration
            .register_companion(
                parent,
                SourceMemberId::new("other.resS").unwrap(),
                companion_fingerprint,
                &mut operation_budget,
            )
            .unwrap();
        let bound = second_registration.commit(&mut operation_budget).unwrap();
        let mut verification_budget = AssetLoadBudget::default();
        let conflicting_binding = VerifiedPhysicalBinding::verify_existing(
            SourceKind::StreamedResource,
            &companion_path,
            companion_fingerprint,
            &mut verification_budget,
        )
        .unwrap();
        let mut begin_budget = AssetLoadBudget::default();
        let mut conflict = bound.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        assert!(matches!(
            conflict.replace_verified_binding(second, conflicting_binding, &mut operation_budget),
            Err(CatalogError::PhysicalOriginConflict { existing, incoming })
                if existing == companion && incoming == second
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
