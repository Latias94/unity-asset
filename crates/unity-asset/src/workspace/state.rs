use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, SourceFingerprint, SourceId, SourceKind, WorkspaceId, WorkspaceRevision,
};

use super::source_catalog::{CatalogError, SourceCatalog};
use super::store::{SourceStore, SourceStoreError};

/// Fully validated immutable workspace baseline.
#[derive(Debug)]
pub(crate) struct WorkspaceState {
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    catalog: SourceCatalog,
    store: SourceStore,
}

impl WorkspaceState {
    pub(crate) fn empty(workspace: WorkspaceId) -> Result<Self, WorkspaceStateError> {
        let mut budget = AssetLoadBudget::default();
        Self::new(
            workspace,
            SourceCatalog::new(workspace),
            SourceStore::new(workspace),
            &mut budget,
        )
    }

    pub(crate) fn new(
        workspace: WorkspaceId,
        catalog: SourceCatalog,
        store: SourceStore,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceStateError> {
        if catalog.workspace() != workspace {
            return Err(WorkspaceStateError::CatalogWorkspaceMismatch {
                expected: workspace,
                actual: catalog.workspace(),
            });
        }
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

        let revision = catalog.revision()?;
        Ok(Self {
            workspace,
            revision,
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
    pub(crate) fn catalog(&self) -> &SourceCatalog {
        &self.catalog
    }

    #[must_use]
    pub(crate) fn store(&self) -> &SourceStore {
        &self.store
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum WorkspaceStateError {
    #[error(transparent)]
    Catalog(Box<CatalogError>),
    #[error(transparent)]
    Store(Box<SourceStoreError>),
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

    use super::*;
    use crate::workspace::source_catalog::{PhysicalOrigin, SourceDescriptor};
    use crate::workspace::store::FrozenSourceParse;
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

        let expected = catalog.revision().unwrap();
        let state = WorkspaceState::new(workspace, catalog, store, &mut AssetLoadBudget::default())
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
            WorkspaceState::new(workspace, catalog, store, &mut AssetLoadBudget::default(),),
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
            WorkspaceState::new(
                workspace,
                SourceCatalog::new(workspace),
                store,
                &mut AssetLoadBudget::default(),
            ),
            Err(WorkspaceStateError::SourceCardinalityMismatch { .. })
        ));
    }
}
