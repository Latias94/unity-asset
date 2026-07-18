use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::mem::{align_of, size_of};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BundleMemberId, ContainmentKind, ContainmentStep, ContractError,
    DigestBuildError, DigestV1, DigestV1Builder, ObjectAddress, ObjectId, ObjectKind, SourceAlias,
    SourceFingerprint, SourceId, SourceKind, SourceLocator, SourceMemberId, WorkspaceId,
    WorkspaceRevision,
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
            SourcePlacement::Member { parent, .. } => Some(*parent),
        }
    }

    #[must_use]
    pub(crate) fn location_kind(&self) -> SourceLocationKind {
        match &self.placement {
            SourcePlacement::Root { .. } => SourceLocationKind::Root,
            SourcePlacement::Member { location_kind, .. } => *location_kind,
        }
    }
}

#[derive(Debug)]
struct SourceRecord {
    descriptor: SourceDescriptor,
    fingerprint: SourceFingerprint,
    source_locator: Arc<SourceLocator>,
    physical_origin: Arc<PhysicalOrigin>,
    canonical_key: Arc<Vec<u8>>,
}

/// Workspace-local authority for source ownership, physical bindings, and opaque identities.
#[derive(Debug)]
pub(crate) struct SourceCatalog {
    workspace: WorkspaceId,
    by_key: HashMap<Arc<Vec<u8>>, SourceId>,
    by_id: BTreeMap<SourceId, Arc<SourceRecord>>,
    by_locator: HashMap<Arc<SourceLocator>, SourceId>,
    physical_roots: HashMap<Arc<PhysicalOrigin>, SourceId>,
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
            physical_roots: HashMap::new(),
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
        mut budget: Option<&mut AssetLoadBudget>,
    ) -> Result<SourceId, CatalogError> {
        if descriptor.kind != fingerprint.kind() {
            return Err(CatalogError::SourceKindMismatch {
                expected: descriptor.kind,
                actual: fingerprint.kind(),
            });
        }

        let (source_locator, physical_origin) =
            self.resolve_placement(&descriptor, budget.as_deref_mut())?;
        let key = canonical_source_key(descriptor.kind, &source_locator, budget.as_deref_mut())?;
        if let Some(existing_source) = self.by_key.get(&key).copied() {
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
            if matches!(&descriptor.placement, SourcePlacement::Root { .. }) {
                let existing_origin = &self
                    .by_id
                    .get(&existing_source)
                    .ok_or(CatalogError::UnknownSource(existing_source))?
                    .physical_origin;
                if existing_origin.as_ref() != physical_origin.as_ref() {
                    return Err(CatalogError::PhysicalOriginChanged {
                        source_id: existing_source,
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

        if matches!(&descriptor.placement, SourcePlacement::Root { .. }) {
            self.ensure_physical_available(source, &physical_origin)?;
        }
        self.reserve_source(&descriptor, budget)?;

        let source_locator = Arc::new(source_locator);
        let key = Arc::new(key);
        match &descriptor.placement {
            SourcePlacement::Root { alias, .. } => {
                self.physical_roots.insert(physical_origin.clone(), source);
                self.root_aliases.insert(Arc::new(alias.clone()), source);
            }
            SourcePlacement::Member { parent, step, .. } => {
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

    fn reserve_source(
        &mut self,
        descriptor: &SourceDescriptor,
        budget: Option<&mut AssetLoadBudget>,
    ) -> Result<(), CatalogError> {
        let retained_bytes = self.checked_source_storage_bytes(descriptor)?;
        if let Some(budget) = budget.as_deref() {
            budget.check_entries(1)?;
            budget.check_bytes(retained_bytes)?;
        }

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
        match &descriptor.placement {
            SourcePlacement::Root { .. } => {
                self.physical_roots.try_reserve(1).map_err(|error| {
                    CatalogError::AllocationFailed {
                        resource: "source catalog physical-root index",
                        requested: 1,
                        unit: CatalogAllocationUnit::Slots,
                        message: error.to_string(),
                    }
                })?;
                self.root_aliases.try_reserve(1).map_err(|error| {
                    CatalogError::AllocationFailed {
                        resource: "source catalog root-alias index",
                        requested: 1,
                        unit: CatalogAllocationUnit::Slots,
                        message: error.to_string(),
                    }
                })?;
            }
            SourcePlacement::Member { parent, .. } => {
                if !self.children_by_parent.contains_key(parent) {
                    self.children_by_parent.try_reserve(1).map_err(|error| {
                        CatalogError::AllocationFailed {
                            resource: "source catalog child index",
                            requested: 1,
                            unit: CatalogAllocationUnit::Slots,
                            message: error.to_string(),
                        }
                    })?;
                    self.children_by_parent.insert(*parent, HashMap::new());
                }
                let parent = descriptor
                    .parent()
                    .ok_or(CatalogError::AllocationSizeOverflow {
                        resource: "source catalog child parent",
                    })?;
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

        if let Some(budget) = budget {
            budget.consume_entries(1)?;
            budget.consume_bytes(retained_bytes)?;
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

        match &descriptor.placement {
            SourcePlacement::Root { alias, .. } => {
                bytes = checked_byte_add(
                    bytes,
                    checked_hash_map_growth_bytes(
                        &self.physical_roots,
                        1,
                        "source catalog physical-root index",
                    )?,
                )?;
                bytes = checked_byte_add(
                    bytes,
                    checked_hash_map_growth_bytes(
                        &self.root_aliases,
                        1,
                        "source catalog root-alias index",
                    )?,
                )?;
                bytes = checked_byte_add(bytes, checked_arc_allocation_bytes::<SourceAlias>()?)?;
                bytes =
                    checked_byte_add(bytes, checked_usize_to_u64(alias.retained_clone_bytes())?)?;
            }
            SourcePlacement::Member { parent, step, .. } => {
                if !self.children_by_parent.contains_key(parent) {
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
                        .get(parent)
                        .ok_or(CatalogError::InvariantMissingChildIndex { parent: *parent })?;
                    bytes = checked_byte_add(
                        bytes,
                        checked_hash_map_growth_bytes(
                            children,
                            1,
                            "source catalog child-step index",
                        )?,
                    )?;
                }
                bytes =
                    checked_byte_add(bytes, checked_arc_allocation_bytes::<ContainmentStep>()?)?;
                bytes = checked_byte_add(
                    bytes,
                    checked_usize_to_u64(step.member().retained_clone_bytes())?,
                )?;
            }
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
            .physical_roots
            .try_reserve(self.physical_roots.len())
            .map_err(|error| CatalogError::AllocationFailed {
                resource: "source catalog transaction physical-root index",
                requested: self.physical_roots.len(),
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
        candidate.physical_roots.extend(
            self.physical_roots
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
                self.physical_roots.len(),
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
                SourceKind::Yaml | SourceKind::SerializedFile | SourceKind::StreamedResource => {
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
        self.physical_roots.get(origin).copied()
    }

    pub(crate) fn physical_origin(
        &self,
        source: SourceId,
    ) -> Result<&PhysicalOrigin, CatalogError> {
        self.ensure_workspace(source)?;
        self.by_id
            .get(&source)
            .map(|record| record.physical_origin.as_ref())
            .ok_or(CatalogError::UnknownSource(source))
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
        const PREFIX: &[u8] = b"unity-asset:source-catalog:v2\0";

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
            digest.update(record.fingerprint.digest().as_bytes())?;
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
                    if !record.source_locator.members().is_empty()
                        || record.source_locator.root_alias() != alias
                        || record.physical_origin.as_ref() != physical_origin
                    {
                        return Err(CatalogError::InvariantRecordMismatch {
                            source_id: *source,
                            field: "root placement",
                        });
                    }
                    let Some((indexed_origin, indexed_source)) = self
                        .physical_roots
                        .get_key_value(record.physical_origin.as_ref())
                    else {
                        return Err(CatalogError::InvariantMissingIndex {
                            source_id: *source,
                            index: "physical origin",
                        });
                    };
                    if indexed_source != source
                        || !Arc::ptr_eq(indexed_origin, &record.physical_origin)
                    {
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
                    ) || record.physical_origin.as_ref()
                        != parent_record.physical_origin.as_ref()
                        || !locator_is_exact_child(
                            record.source_locator.as_ref(),
                            parent_record.source_locator.as_ref(),
                            step,
                        )
                    {
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
                if !matches!(
                    &record.descriptor.placement,
                    SourcePlacement::Member {
                        parent: actual_parent,
                        step: actual_step,
                        ..
                    } if actual_parent == parent && actual_step == step.as_ref()
                ) {
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
        let child_count = self.by_id.len().saturating_sub(root_count);
        let indexed_children = self
            .children_by_parent
            .values()
            .map(HashMap::len)
            .sum::<usize>();
        if self.root_aliases.len() != root_count
            || self.physical_roots.len() != root_count
            || indexed_children != child_count
        {
            return Err(CatalogError::InvariantOwnershipIndexCardinality {
                roots: root_count,
                root_aliases: self.root_aliases.len(),
                physical_roots: self.physical_roots.len(),
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
            match &record.descriptor.placement {
                SourcePlacement::Root { alias, .. } => {
                    self.physical_roots.remove(&record.physical_origin);
                    self.root_aliases.remove(alias);
                }
                SourcePlacement::Member { parent, step, .. } => {
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
        budget: Option<&mut AssetLoadBudget>,
    ) -> Result<(SourceLocator, Arc<PhysicalOrigin>), CatalogError> {
        let planned_bytes = self.checked_placement_bytes(descriptor)?;
        if let Some(budget) = budget.as_deref() {
            budget.check_bytes(planned_bytes)?;
        }
        if let Some(budget) = budget {
            budget.consume_bytes(planned_bytes)?;
        }

        match &descriptor.placement {
            SourcePlacement::Root {
                alias,
                physical_origin,
            } => Ok((
                SourceLocator::path(alias.as_str()).map_err(CatalogError::InvalidIdentity)?,
                Arc::new(physical_origin.clone()),
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
                Ok((source_locator, Arc::clone(&parent_record.physical_origin)))
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

    pub(crate) fn remove_subtree(
        &mut self,
        root: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SourceId>, CatalogError> {
        self.ensure_active()?;
        match self.candidate.remove_subtree(root, budget) {
            Ok(removed) => Ok(removed),
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

    pub(crate) fn commit(self) -> Result<SourceCatalog, CatalogError> {
        if self.failed {
            return Err(CatalogError::TransactionAborted);
        }
        self.candidate.validate()?;
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

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum CatalogError {
    #[error("invalid source identity: {0}")]
    InvalidIdentity(ContractError),
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
        "source catalog ownership index mismatch: roots={roots}, aliases={root_aliases}, physical={physical_roots}, children={children}, indexed_children={indexed_children}"
    )]
    InvariantOwnershipIndexCardinality {
        roots: usize,
        root_aliases: usize,
        physical_roots: usize,
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

fn ensure_regular_member_kind(kind: SourceKind) -> Result<(), CatalogError> {
    if kind == SourceKind::StreamedResource {
        Err(CatalogError::StreamedResourceRequiresSidecar)
    } else {
        Ok(())
    }
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

fn canonical_source_key(
    kind: SourceKind,
    locator: &SourceLocator,
    budget: Option<&mut AssetLoadBudget>,
) -> Result<Vec<u8>, CatalogError> {
    let capacity = canonical_source_key_len(kind, locator)?;
    if let Some(budget) = budget.as_deref() {
        budget.check_bytes(u64::try_from(capacity).map_err(|_| {
            CatalogError::AllocationSizeOverflow {
                resource: "canonical source key",
            }
        })?)?;
    }
    let mut key = Vec::new();
    key.try_reserve_exact(capacity)
        .map_err(|error| CatalogError::AllocationFailed {
            resource: "canonical source key",
            requested: capacity,
            unit: CatalogAllocationUnit::Bytes,
            message: error.to_string(),
        })?;
    if let Some(budget) = budget {
        budget.consume_bytes(u64::try_from(capacity).map_err(|_| {
            CatalogError::AllocationSizeOverflow {
                resource: "canonical source key",
            }
        })?)?;
    }
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
    let mut length = b"unity-asset:source:v2\0".len();
    length = checked_usize_add(length, size_of::<u64>())?;
    length = checked_usize_add(length, kind.tag().len())?;
    length = checked_usize_add(length, size_of::<u64>())?;
    length = checked_usize_add(length, locator.root_alias().as_str().len())?;
    length = checked_usize_add(length, size_of::<u64>())?;
    for step in locator.members() {
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
    let bytes = size_of::<T>()
        .checked_add(size_of::<usize>().saturating_mul(2))
        .and_then(|value| value.checked_add(align_of::<T>().max(align_of::<usize>())))
        .ok_or(CatalogError::AllocationSizeOverflow {
            resource: "source catalog Arc allocation",
        })?;
    checked_usize_to_u64(bytes)
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

    fn root_descriptor(kind: SourceKind, alias: &str, contents: &[u8]) -> SourceDescriptor {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(alias.replace('/', "-"));
        fs::write(&path, contents).unwrap();
        SourceDescriptor::root(
            kind,
            SourceAlias::new(alias).unwrap(),
            PhysicalOrigin::from_existing_path(path).unwrap(),
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
            transaction.commit(),
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
        let candidate = transaction.commit().unwrap();

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
        let candidate = transaction.commit().unwrap();

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
            .commit()
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
        let placement_bytes = catalog.checked_placement_bytes(&descriptor).unwrap();
        let (locator, _) = catalog.resolve_placement(&descriptor, None).unwrap();
        let key_bytes = canonical_source_key_len(descriptor.kind(), &locator).unwrap() as u64;
        let storage_bytes = catalog.checked_source_storage_bytes(&descriptor).unwrap();
        let allowed = placement_bytes
            .checked_add(key_bytes)
            .and_then(|value| value.checked_add(storage_bytes))
            .unwrap()
            - 1;
        let mut budget = budget_with(allowed, 1);

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
        assert_eq!(catalog.physical_roots.capacity(), 0);
        assert_eq!(catalog.root_aliases.capacity(), 0);
        assert_eq!(budget.usage().bytes, placement_bytes + key_bytes);
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
        let placement_bytes = catalog.checked_placement_bytes(&descriptor).unwrap();
        let (locator, _) = catalog.resolve_placement(&descriptor, None).unwrap();
        let key_bytes = canonical_source_key_len(descriptor.kind(), &locator).unwrap() as u64;
        let storage_bytes = catalog.checked_source_storage_bytes(&descriptor).unwrap();
        let allowed = placement_bytes + key_bytes + storage_bytes - 1;
        let mut budget = budget_with(allowed, 1);

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
        assert_eq!(budget.usage().bytes, placement_bytes + key_bytes);
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
