use thiserror::Error;
use unity_asset_binary::typetree::TypeTreeParseMode;
use unity_asset_core::{
    AssetLoadBudget, DigestBuildError, DigestV1, DigestV1Builder, SourceFingerprint, SourceId,
    SourceKind, WorkspaceId, WorkspaceRevision,
};

use super::source_catalog::{CatalogError, SourceCatalog, SourceLocationKind};

mod store;
mod transaction;

#[cfg(test)]
pub(crate) use store::TestSourceBackingOwner;
pub(crate) use store::{
    FrozenSourceParse, SourceEntry, SourceStore, SourceStoreError, WeakSourceBackingOwner,
};
pub(super) use transaction::{
    PreparedSourceChild, PreparedSourceRelation, PreparedSourceTree, PreparedWorkspaceState,
    VerifiedSourceContent, WorkspaceStateInstallOutcome, WorkspaceStateTransaction,
};

/// Digest of every runtime source-to-physical-origin binding in one workspace state.
///
/// This identity complements [`WorkspaceRevision`]: the revision describes logical content and
/// object identity, while this digest proves the complete physical installation used by durable
/// commit and recovery.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct WorkspaceInstallationDigest(DigestV1);

impl WorkspaceInstallationDigest {
    #[must_use]
    pub const fn new(digest: DigestV1) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn digest(self) -> DigestV1 {
        self.0
    }

    fn from_catalog(catalog: &SourceCatalog) -> Result<Self, WorkspaceStateError> {
        catalog.installation_digest().map(Self).map_err(Into::into)
    }
}

/// Fully validated immutable workspace baseline.
#[derive(Debug)]
pub(crate) struct WorkspaceState {
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    installation: WorkspaceInstallationDigest,
    parse_context: DigestV1,
    typetree_mode: TypeTreeParseMode,
    catalog: SourceCatalog,
    store: SourceStore,
}

impl WorkspaceState {
    pub(crate) fn empty(
        workspace: WorkspaceId,
        typetree_mode: TypeTreeParseMode,
    ) -> Result<Self, WorkspaceStateError> {
        let mut budget = AssetLoadBudget::default();
        Self::from_candidates(
            workspace,
            typetree_mode,
            SourceCatalog::new(workspace),
            SourceStore::new(workspace),
            &mut budget,
        )
    }

    fn from_candidates(
        workspace: WorkspaceId,
        typetree_mode: TypeTreeParseMode,
        catalog: SourceCatalog,
        store: SourceStore,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceStateError> {
        ensure_catalog_workspace(workspace, &catalog)?;
        if store.workspace() != workspace {
            return Err(WorkspaceStateError::StoreWorkspaceMismatch {
                expected: workspace,
                actual: store.workspace(),
            });
        }
        catalog.validate()?;
        store.validate(budget)?;

        if catalog.len() != store.len() {
            return Err(WorkspaceStateError::SourceCardinalityMismatch {
                catalog: catalog.len(),
                store: store.len(),
            });
        }

        for (source, descriptor) in catalog.iter() {
            if descriptor.location_kind() == SourceLocationKind::Companion
                && catalog.physical_origin_option(source)?.is_none()
            {
                return Err(WorkspaceStateError::MissingCompanionPhysicalOrigin(source));
            }
            let entry = store
                .get(source)
                .ok_or(WorkspaceStateError::MissingSourceImage(source))?;
            if descriptor.kind() != entry.image().kind() {
                return Err(WorkspaceStateError::SourceKindMismatch {
                    source_id: source,
                    catalog: descriptor.kind(),
                    store: entry.image().kind(),
                });
            }
            match descriptor.kind() {
                SourceKind::SerializedFile if entry.cached_serialized().is_none() => {
                    return Err(WorkspaceStateError::MissingFrozenParse {
                        source_id: source,
                        kind: SourceKind::SerializedFile,
                    });
                }
                SourceKind::Yaml if entry.cached_yaml().is_none() => {
                    return Err(WorkspaceStateError::MissingFrozenParse {
                        source_id: source,
                        kind: SourceKind::Yaml,
                    });
                }
                SourceKind::SerializedFile
                | SourceKind::Yaml
                | SourceKind::AssetBundle
                | SourceKind::WebFile
                | SourceKind::Archive
                | SourceKind::StreamedResource => {}
            }
            let catalog_fingerprint = catalog.fingerprint(source)?;
            let store_fingerprint = entry.image().fingerprint();
            if catalog_fingerprint != store_fingerprint {
                return Err(WorkspaceStateError::SourceFingerprintMismatch {
                    source_id: source,
                    catalog: catalog_fingerprint,
                    store: store_fingerprint,
                });
            }
            if let Some(parent) = catalog.parent(source)?
                && !store.contains(parent)
            {
                return Err(WorkspaceStateError::MissingParentImage {
                    source_id: source,
                    parent,
                });
            }
        }
        for (source, _) in store.iter() {
            if !catalog.contains(source) {
                return Err(WorkspaceStateError::MissingCatalogRecord(source));
            }
        }

        let parse_context = parse_context_digest(&store, typetree_mode)?;
        let revision = workspace_revision(&catalog, parse_context)?;
        let installation = WorkspaceInstallationDigest::from_catalog(&catalog)?;
        Ok(Self {
            workspace,
            revision,
            installation,
            parse_context,
            typetree_mode,
            catalog,
            store,
        })
    }

    #[must_use]
    pub(crate) const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub(crate) const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub(crate) const fn installation(&self) -> WorkspaceInstallationDigest {
        self.installation
    }

    #[must_use]
    pub(crate) const fn typetree_mode(&self) -> TypeTreeParseMode {
        self.typetree_mode
    }

    pub(crate) fn revision_for_catalog(
        &self,
        catalog: &SourceCatalog,
    ) -> Result<WorkspaceRevision, WorkspaceStateError> {
        ensure_catalog_workspace(self.workspace, catalog)?;
        workspace_revision(catalog, self.parse_context)
    }

    pub(crate) fn installation_for_catalog(
        &self,
        catalog: &SourceCatalog,
    ) -> Result<WorkspaceInstallationDigest, WorkspaceStateError> {
        ensure_catalog_workspace(self.workspace, catalog)?;
        WorkspaceInstallationDigest::from_catalog(catalog)
    }

    /// Compares every installed binding that is intentionally excluded from revision identity.
    ///
    /// Revision equality alone is insufficient: relocating an otherwise identical root keeps the
    /// same logical revision but must install the new physical binding.
    fn installation_equivalent(&self, other: &Self) -> bool {
        self.installation == other.installation
    }

    #[must_use]
    pub(crate) fn catalog(&self) -> &SourceCatalog {
        &self.catalog
    }

    #[must_use]
    pub(crate) fn store(&self) -> &SourceStore {
        &self.store
    }
}

fn ensure_catalog_workspace(
    expected: WorkspaceId,
    catalog: &SourceCatalog,
) -> Result<(), WorkspaceStateError> {
    if catalog.workspace() != expected {
        return Err(WorkspaceStateError::CatalogWorkspaceMismatch {
            expected,
            actual: catalog.workspace(),
        });
    }
    Ok(())
}

fn workspace_revision(
    catalog: &SourceCatalog,
    parse_context: DigestV1,
) -> Result<WorkspaceRevision, WorkspaceStateError> {
    const PREFIX: &[u8] = b"unity-asset:workspace-state:v1\0";

    let catalog_revision = catalog.revision()?;
    let logical_length = u64::try_from(PREFIX.len())
        .map_err(|_| DigestBuildError::LengthOverflow)?
        .checked_add((DigestV1::BYTE_LEN as u64) * 2)
        .ok_or(DigestBuildError::LengthOverflow)?;
    let mut digest = DigestV1Builder::new(logical_length);
    digest.update(PREFIX)?;
    digest.update(catalog_revision.digest().as_bytes())?;
    digest.update(parse_context.as_bytes())?;
    Ok(WorkspaceRevision::new(digest.finalize()?))
}

fn parse_context_digest(
    store: &SourceStore,
    typetree_mode: TypeTreeParseMode,
) -> Result<DigestV1, WorkspaceStateError> {
    const PREFIX: &[u8] = b"unity-asset:workspace-parse-context:v1\0";

    let mut registry_count = 0_u64;
    let mut logical_length = u64::try_from(PREFIX.len())
        .map_err(|_| DigestBuildError::LengthOverflow)?
        .checked_add(1)
        .and_then(|length| length.checked_add(8))
        .ok_or(DigestBuildError::LengthOverflow)?;
    for (source, entry) in store.iter() {
        if frozen_registry_digest(source, entry)?.is_none() {
            continue;
        }
        registry_count = registry_count
            .checked_add(1)
            .ok_or(DigestBuildError::LengthOverflow)?;
        let kind_length = DigestV1Builder::framed_len(source.kind().tag().as_bytes())?;
        logical_length = logical_length
            .checked_add(16)
            .and_then(|length| length.checked_add(kind_length))
            .and_then(|length| length.checked_add(DigestV1::BYTE_LEN as u64))
            .ok_or(DigestBuildError::LengthOverflow)?;
    }

    let mut digest = DigestV1Builder::new(logical_length);
    digest.update(PREFIX)?;
    digest.update(&[match typetree_mode {
        TypeTreeParseMode::Strict => 0,
        TypeTreeParseMode::Lenient => 1,
    }])?;
    digest.update(&registry_count.to_le_bytes())?;
    for (source, entry) in store.iter() {
        let Some(registry_digest) = frozen_registry_digest(source, entry)? else {
            continue;
        };
        digest.update(&source.local().to_le_bytes())?;
        digest.update_framed(source.kind().tag().as_bytes())?;
        digest.update(registry_digest.as_bytes())?;
    }
    digest.finalize().map_err(Into::into)
}

fn frozen_registry_digest(
    source: SourceId,
    entry: &SourceEntry,
) -> Result<Option<DigestV1>, WorkspaceStateError> {
    let Some(serialized) = entry.cached_serialized() else {
        return Ok(None);
    };
    serialized
        .type_tree_registry()
        .map(|registry| {
            registry
                .semantic_digest()
                .ok_or(WorkspaceStateError::UnidentifiedTypeTreeRegistry { source_id: source })
        })
        .transpose()
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum WorkspaceStateError {
    #[error(transparent)]
    Budget(#[from] unity_asset_core::BudgetError),
    #[error(transparent)]
    Catalog(Box<CatalogError>),
    #[error(transparent)]
    Store(Box<SourceStoreError>),
    #[error(transparent)]
    Digest(#[from] DigestBuildError),
    #[error("workspace state expected catalog {expected}, got {actual}")]
    CatalogWorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("workspace state expected source store {expected}, got {actual}")]
    StoreWorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("workspace source cardinality mismatch: catalog={catalog}, store={store}")]
    SourceCardinalityMismatch { catalog: usize, store: usize },
    #[error("source catalog has no image for {0:?}")]
    MissingSourceImage(SourceId),
    #[error("companion source {0:?} has no authoritative physical origin")]
    MissingCompanionPhysicalOrigin(SourceId),
    #[error("source store has no catalog record for {0:?}")]
    MissingCatalogRecord(SourceId),
    #[error("source {source_id:?} has catalog kind {catalog:?}, store kind {store:?}")]
    SourceKindMismatch {
        source_id: SourceId,
        catalog: SourceKind,
        store: SourceKind,
    },
    #[error("source {source_id:?} has catalog fingerprint {catalog}, store fingerprint {store}")]
    SourceFingerprintMismatch {
        source_id: SourceId,
        catalog: SourceFingerprint,
        store: SourceFingerprint,
    },
    #[error("source {source_id:?} has no image for its parent {parent:?}")]
    MissingParentImage {
        source_id: SourceId,
        parent: SourceId,
    },
    #[error("source {source_id:?} of kind {kind:?} has no frozen parse")]
    MissingFrozenParse {
        source_id: SourceId,
        kind: SourceKind,
    },
    #[error("source {source_id:?} retains a TypeTree registry without a stable semantic digest")]
    UnidentifiedTypeTreeRegistry { source_id: SourceId },
    #[error("workspace state transaction is already aborted")]
    TransactionAborted,
}

impl From<CatalogError> for WorkspaceStateError {
    fn from(error: CatalogError) -> Self {
        Self::Catalog(Box::new(error))
    }
}

impl From<SourceStoreError> for WorkspaceStateError {
    fn from(error: SourceStoreError) -> Self {
        Self::Store(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use unity_asset_core::{AssetLoadBudget, SourceAlias};

    use super::FrozenSourceParse;
    use super::*;
    use crate::workspace::source_catalog::{PhysicalOrigin, SourceDescriptor};
    use unity_asset_core::VerifiedSourceImage;

    fn root_descriptor(kind: SourceKind, alias: &str, bytes: &[u8]) -> SourceDescriptor {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(alias.replace('/', "-"));
        fs::write(&path, bytes).unwrap();
        let origin = PhysicalOrigin::from_existing_path(&path).unwrap();
        // PhysicalOrigin is canonical and owned, so the temporary directory may be released.
        SourceDescriptor::root(kind, SourceAlias::new(alias).unwrap(), origin)
    }

    #[test]
    fn validated_state_caches_the_catalog_revision() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let bytes = b"serialized";
        let mut catalog = SourceCatalog::new(workspace);
        let source = catalog
            .register(
                root_descriptor(SourceKind::Archive, "main.zip", bytes),
                SourceFingerprint::from_bytes(SourceKind::Archive, bytes),
            )
            .unwrap();
        let mut store = SourceStore::new(workspace);
        store
            .insert(
                source,
                VerifiedSourceImage::verify(SourceKind::Archive, bytes.to_vec().into()),
                FrozenSourceParse::None,
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        let parse_context = parse_context_digest(&store, TypeTreeParseMode::Lenient).unwrap();
        let expected = workspace_revision(&catalog, parse_context).unwrap();
        let state = WorkspaceState::from_candidates(
            workspace,
            TypeTreeParseMode::Lenient,
            catalog,
            store,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(state.revision(), expected);
        assert_eq!(state.workspace(), workspace);
    }

    #[test]
    fn state_rejects_catalog_store_fingerprint_drift() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let source = catalog
            .register(
                root_descriptor(SourceKind::Archive, "main.zip", b"catalog"),
                SourceFingerprint::from_bytes(SourceKind::Archive, b"catalog"),
            )
            .unwrap();
        let mut store = SourceStore::new(workspace);
        store
            .insert(
                source,
                VerifiedSourceImage::verify(SourceKind::Archive, b"store".to_vec().into()),
                FrozenSourceParse::None,
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert!(matches!(
            WorkspaceState::from_candidates(
                workspace,
                TypeTreeParseMode::Lenient,
                catalog,
                store,
                &mut AssetLoadBudget::default(),
            ),
            Err(WorkspaceStateError::SourceFingerprintMismatch { .. })
        ));
    }

    #[test]
    fn state_rejects_an_orphan_store_entry() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let source = SourceId::new(workspace, SourceKind::Archive, 1).unwrap();
        let mut store = SourceStore::new(workspace);
        store
            .insert(
                source,
                VerifiedSourceImage::verify(SourceKind::Archive, b"orphan".to_vec().into()),
                FrozenSourceParse::None,
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert!(matches!(
            WorkspaceState::from_candidates(
                workspace,
                TypeTreeParseMode::Lenient,
                SourceCatalog::new(workspace),
                store,
                &mut AssetLoadBudget::default(),
            ),
            Err(WorkspaceStateError::SourceCardinalityMismatch { .. })
        ));
    }
}
