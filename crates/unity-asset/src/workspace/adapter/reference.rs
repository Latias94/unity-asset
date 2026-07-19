use unity_asset_core::{
    AssetLoadBudget, BudgetError, ObjectAddress, ObjectId, SourceKind, WorkspaceId,
    WorkspaceRevision,
};

use crate::reference::input::{ReferenceInput, ReferenceSource, ReferenceSourceOwner, sealed};
use crate::reference::{ReferenceGraphError, ReferenceStore};

use super::super::{ReferenceViewParts, WorkspaceError};

impl sealed::Sealed for ReferenceViewParts<'_> {}

impl ReferenceInput for ReferenceViewParts<'_> {
    fn workspace_id(&self) -> WorkspaceId {
        self.state.workspace()
    }

    fn revision(&self) -> WorkspaceRevision {
        self.state.revision()
    }

    fn object_source_count(&self) -> usize {
        self.state
            .store()
            .iter()
            .filter(|(source, _)| is_object_source(source.kind()))
            .count()
    }

    fn object_sources(
        &self,
    ) -> impl Iterator<Item = Result<ReferenceSource<'_>, ReferenceGraphError>> {
        self.state
            .store()
            .iter()
            .filter(|(source, _)| is_object_source(source.kind()))
            .map(|(source, entry)| {
                let catalog = self.state.catalog();
                let locator = catalog
                    .source_locator(source)
                    .map_err(WorkspaceError::from)?;
                let parent = catalog.parent(source).map_err(WorkspaceError::from)?;
                let physical_path = parent
                    .is_none()
                    .then(|| catalog.physical_origin(source))
                    .transpose()
                    .map_err(WorkspaceError::from)?
                    .map(|origin| origin.path());
                let fingerprint = catalog.fingerprint(source).map_err(WorkspaceError::from)?;
                let owner = ReferenceSourceOwner::from(entry.image().backing());
                match source.kind() {
                    SourceKind::SerializedFile => ReferenceSource::serialized(
                        source,
                        fingerprint,
                        owner,
                        locator,
                        parent,
                        physical_path,
                        entry
                            .cached_serialized()
                            .ok_or(ReferenceGraphError::Invariant(
                                "SerializedFile source has no frozen parse",
                            ))?
                            .as_ref(),
                    ),
                    SourceKind::Yaml => ReferenceSource::yaml(
                        source,
                        fingerprint,
                        owner,
                        locator,
                        parent,
                        physical_path,
                        entry
                            .cached_yaml()
                            .ok_or(ReferenceGraphError::Invariant(
                                "YAML source has no frozen parse",
                            ))?
                            .as_ref(),
                    ),
                    SourceKind::AssetBundle
                    | SourceKind::WebFile
                    | SourceKind::Archive
                    | SourceKind::StreamedResource => Err(ReferenceGraphError::Invariant(
                        "non-object source reached the reference input adapter",
                    )),
                }
            })
    }

    fn reference_store(&self) -> &ReferenceStore {
        self.store.as_ref()
    }

    fn typetree_options(&self) -> unity_asset_binary::typetree::TypeTreeParseOptions {
        self.typetree
    }

    fn address_for_object(
        &self,
        object: &ObjectId,
        budget: &mut AssetLoadBudget,
    ) -> Result<ObjectAddress, ReferenceGraphError> {
        let locator_bytes = self
            .state
            .catalog()
            .source_locator(object.source())
            .map_err(WorkspaceError::from)?
            .retained_clone_bytes()
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "reference target address",
            })?;
        let retained = locator_bytes
            .checked_add(object.retained_clone_bytes())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "reference target address",
            })?;
        budget.consume_bytes(usize_to_u64(retained, "reference target address")?)?;
        self.state
            .catalog()
            .address_for_object(object)
            .map_err(WorkspaceError::from)
            .map_err(ReferenceGraphError::from)
    }
}

const fn is_object_source(kind: SourceKind) -> bool {
    matches!(kind, SourceKind::SerializedFile | SourceKind::Yaml)
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, BudgetError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource })
}
