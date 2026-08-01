use std::sync::Arc;

use unity_asset_core::{
    AssetLoadBudget, BudgetedSourceBytes, BudgetedVerifiedSourceImage, SourceFingerprint, SourceId,
    SourceKind, SourceMemberId, WorkspaceRevision, arc_value_allocation_bytes,
};

use super::store::{FrozenSourceParse, SourceStore};
use super::{WorkspaceState, WorkspaceStateError};
use crate::workspace::inspection::WorkspaceSourceFormatInspection;
use crate::workspace::source_catalog::{
    PhysicalDomainChange, PhysicalOrigin, RootAdmissionDecision, SourceCatalog, SourceDescriptor,
};

/// Containment relation used to derive a child descriptor after its parent is registered.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PreparedSourceRelation {
    Archive,
    Bundle,
    WebFile,
}

/// One prepared child whose final identity is derived inside the state transaction.
#[derive(Debug)]
pub(crate) struct PreparedSourceChild {
    relation: PreparedSourceRelation,
    identity: SourceMemberId,
    source: PreparedSourceTree,
}

impl PreparedSourceChild {
    pub(crate) fn new(
        relation: PreparedSourceRelation,
        identity: SourceMemberId,
        source: PreparedSourceTree,
    ) -> Self {
        Self {
            relation,
            identity,
            source,
        }
    }
}

/// Budget-owned immutable content and its proven parse/inspection tree.
///
/// Preparation verifies the source once, then retains its budget-domain proof until registration
/// consumes the image. Parse state, format inspection, and child topology travel with that proof so
/// no caller can publish unaccounted backing through the state transaction.
#[derive(Debug)]
pub(crate) struct PreparedSourceTree {
    image: BudgetedVerifiedSourceImage,
    parse: FrozenSourceParse,
    format: WorkspaceSourceFormatInspection,
    children: Vec<PreparedSourceChild>,
}

impl PreparedSourceTree {
    pub(crate) fn new(
        kind: SourceKind,
        image: BudgetedSourceBytes,
        parse: FrozenSourceParse,
        format: WorkspaceSourceFormatInspection,
        children: Vec<PreparedSourceChild>,
    ) -> Self {
        Self {
            image: image.verify(kind),
            parse,
            format,
            children,
        }
    }

    #[must_use]
    pub(crate) const fn kind(&self) -> SourceKind {
        self.image.kind()
    }

    #[must_use]
    pub(crate) const fn fingerprint(&self) -> SourceFingerprint {
        self.image.fingerprint()
    }
}

/// Opaque source replacement whose backing is owned by the caller's budget domain.
#[derive(Debug)]
pub(crate) struct VerifiedSourceContent {
    source: SourceId,
    image: BudgetedVerifiedSourceImage,
    parse: FrozenSourceParse,
    format: WorkspaceSourceFormatInspection,
}

impl VerifiedSourceContent {
    pub(crate) fn from_budgeted(
        source: SourceId,
        image: BudgetedVerifiedSourceImage,
        parse: FrozenSourceParse,
        format: WorkspaceSourceFormatInspection,
    ) -> Self {
        debug_assert_eq!(source.kind(), image.kind());
        Self {
            source,
            image,
            parse,
            format,
        }
    }
}

/// Fully validated immutable state and the exact state it may replace.
#[derive(Debug)]
pub(crate) struct PreparedWorkspaceState {
    expected: Arc<WorkspaceState>,
    next: Arc<WorkspaceState>,
}

impl PreparedWorkspaceState {
    #[must_use]
    pub(crate) fn revision(&self) -> WorkspaceRevision {
        self.next.revision()
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn changed(&self) -> bool {
        !Arc::ptr_eq(&self.expected, &self.next)
    }

    pub(crate) fn install_into(
        &self,
        current: &mut Arc<WorkspaceState>,
    ) -> WorkspaceStateInstallOutcome {
        if Arc::ptr_eq(current, &self.next) {
            return WorkspaceStateInstallOutcome::Unchanged;
        }
        if !Arc::ptr_eq(current, &self.expected) {
            return WorkspaceStateInstallOutcome::Stale;
        }
        *current = Arc::clone(&self.next);
        WorkspaceStateInstallOutcome::Installed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceStateInstallOutcome {
    Installed,
    Unchanged,
    Stale,
}

/// The only authoritative mutation boundary for a committed workspace state.
pub(crate) struct WorkspaceStateTransaction {
    expected: Arc<WorkspaceState>,
    catalog: crate::workspace::source_catalog::SourceCatalogTransaction,
    store: SourceStore,
    failed: bool,
}

impl WorkspaceStateTransaction {
    pub(crate) fn begin(
        expected: Arc<WorkspaceState>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceStateError> {
        let catalog = expected.catalog().begin_state_transaction(budget)?;
        Self::from_catalog_transaction(expected, catalog, budget)
    }

    pub(crate) fn begin_with_catalog(
        expected: Arc<WorkspaceState>,
        catalog: &SourceCatalog,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceStateError> {
        let catalog = catalog.begin_state_transaction(budget)?;
        Self::from_catalog_transaction(expected, catalog, budget)
    }

    fn from_catalog_transaction(
        expected: Arc<WorkspaceState>,
        catalog: crate::workspace::source_catalog::SourceCatalogTransaction,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceStateError> {
        let store = expected.store().clone_for_update(budget)?;
        Ok(Self {
            expected,
            catalog,
            store,
            failed: false,
        })
    }

    pub(crate) fn root_admission_decision(
        &mut self,
        alias: &unity_asset_core::SourceAlias,
        origin: &PhysicalOrigin,
        fingerprint: SourceFingerprint,
    ) -> Result<RootAdmissionDecision, WorkspaceStateError> {
        self.ensure_active()?;
        self.catalog
            .root_admission_decision(alias, origin, fingerprint)
            .map_err(Into::into)
    }

    pub(crate) fn is_root(&mut self, source: SourceId) -> Result<bool, WorkspaceStateError> {
        self.ensure_active()?;
        self.catalog.is_root(source).map_err(Into::into)
    }

    pub(crate) fn register_tree(
        &mut self,
        descriptor: SourceDescriptor,
        tree: PreparedSourceTree,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceStateError> {
        self.ensure_active()?;
        match self.register_tree_inner(descriptor, tree, budget) {
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
    ) -> Result<Vec<SourceId>, WorkspaceStateError> {
        self.ensure_active()?;
        let removed = match self.catalog.remove_subtree(root, budget) {
            Ok(removed) => removed,
            Err(error) => {
                self.failed = true;
                return Err(error.into());
            }
        };
        if let Err(error) = self.store.remove_sorted(&removed) {
            self.failed = true;
            return Err(error.into());
        }
        Ok(removed)
    }

    pub(crate) fn rewrite_physical_domains_from_changes(
        &mut self,
        changes: &[PhysicalDomainChange],
        budget: &mut AssetLoadBudget,
    ) -> Result<(), WorkspaceStateError> {
        self.ensure_active()?;
        match self
            .catalog
            .rewrite_physical_domains_from_changes(changes, budget)
        {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failed = true;
                Err(error.into())
            }
        }
    }

    /// Registers metadata whose verified content is supplied later in the same transaction.
    ///
    /// Recovery needs this two-step form because physical-domain ownership is resolved only after
    /// every journal-declared source exists. Commit rejects the transaction unless matching
    /// content is installed before final validation.
    pub(crate) fn register_descriptor(
        &mut self,
        descriptor: SourceDescriptor,
        fingerprint: SourceFingerprint,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceStateError> {
        self.ensure_active()?;
        match self.catalog.register(descriptor, fingerprint, budget) {
            Ok(source) => Ok(source),
            Err(error) => {
                self.failed = true;
                Err(error.into())
            }
        }
    }

    pub(crate) fn replace_verified_content(
        &mut self,
        content: VerifiedSourceContent,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), WorkspaceStateError> {
        self.ensure_active()?;
        self.insert_content(content, budget).map(|_| ())
    }

    #[must_use]
    pub(crate) fn content_fingerprint(&self, source: SourceId) -> Option<SourceFingerprint> {
        self.store
            .get(source)
            .map(|entry| entry.image().fingerprint())
    }

    pub(crate) fn commit(
        self,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedWorkspaceState, WorkspaceStateError> {
        if self.failed {
            return Err(WorkspaceStateError::TransactionAborted);
        }
        let catalog = self.catalog.into_state_candidate()?;
        let next = WorkspaceState::from_candidates(
            self.expected.workspace(),
            self.expected.typetree_mode(),
            catalog,
            self.store,
            budget,
        )?;
        if next.revision() == self.expected.revision()
            && next.installation_equivalent(&self.expected)
        {
            return Ok(PreparedWorkspaceState {
                next: Arc::clone(&self.expected),
                expected: self.expected,
            });
        }

        let retained = arc_value_allocation_bytes::<WorkspaceState>().map_err(|_| {
            unity_asset_core::BudgetError::ArithmeticOverflow {
                resource: "workspace state",
            }
        })?;
        budget.check_bytes(retained)?;
        let next = Arc::new(next);
        budget.consume_bytes(retained)?;
        Ok(PreparedWorkspaceState {
            expected: self.expected,
            next,
        })
    }

    fn register_tree_inner(
        &mut self,
        descriptor: SourceDescriptor,
        tree: PreparedSourceTree,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceStateError> {
        let PreparedSourceTree {
            image,
            parse,
            format,
            children,
        } = tree;
        let fingerprint = image.fingerprint();
        let source = self.catalog.register(descriptor, fingerprint, budget)?;
        let content = VerifiedSourceContent::from_budgeted(source, image, parse, format);
        self.insert_content(content, budget)?;

        for child in children {
            let descriptor =
                child_descriptor(source, child.relation, child.source.kind(), child.identity)?;
            self.register_tree_inner(descriptor, child.source, budget)?;
        }
        Ok(source)
    }

    fn insert_content(
        &mut self,
        content: VerifiedSourceContent,
        budget: &mut AssetLoadBudget,
    ) -> Result<Arc<super::store::SourceEntry>, WorkspaceStateError> {
        let result = (|| {
            let image = content.image.into_image(budget)?;
            self.store
                .insert_proven(content.source, image, content.parse, content.format, budget)
                .map_err(Into::into)
        })();
        match result {
            Ok(entry) => Ok(entry),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn ensure_active(&self) -> Result<(), WorkspaceStateError> {
        if self.failed {
            Err(WorkspaceStateError::TransactionAborted)
        } else {
            Ok(())
        }
    }
}

fn child_descriptor(
    parent: SourceId,
    relation: PreparedSourceRelation,
    kind: SourceKind,
    identity: SourceMemberId,
) -> Result<SourceDescriptor, WorkspaceStateError> {
    if kind == SourceKind::StreamedResource {
        return SourceDescriptor::sidecar(parent, identity).map_err(Into::into);
    }
    match relation {
        PreparedSourceRelation::Archive => SourceDescriptor::archive_member(parent, kind, identity),
        PreparedSourceRelation::Bundle => SourceDescriptor::bundle_member(parent, kind, identity),
        PreparedSourceRelation::WebFile => SourceDescriptor::webfile_member(parent, kind, identity),
    }
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use unity_asset_binary::typetree::TypeTreeParseMode;
    use unity_asset_core::{BudgetError, SourceAlias, WorkspaceId};

    use super::*;
    use crate::workspace::inspection::WorkspaceSourceFormatInspection;

    fn empty_state() -> Arc<WorkspaceState> {
        Arc::new(
            WorkspaceState::empty(
                WorkspaceId::from_u128(1).expect("valid workspace"),
                TypeTreeParseMode::Lenient,
            )
            .expect("empty state"),
        )
    }

    fn root_descriptor(path: &std::path::Path, alias: &str) -> SourceDescriptor {
        SourceDescriptor::root(
            SourceKind::StreamedResource,
            SourceAlias::new(alias).expect("valid alias"),
            PhysicalOrigin::from_existing_path(path).expect("physical origin"),
        )
    }

    fn prepared_stream(bytes: &[u8], budget: &mut AssetLoadBudget) -> PreparedSourceTree {
        PreparedSourceTree::new(
            SourceKind::StreamedResource,
            BudgetedSourceBytes::from_arc(Arc::from(bytes), budget).expect("budgeted source"),
            FrozenSourceParse::None,
            WorkspaceSourceFormatInspection::StreamedResource,
            Vec::new(),
        )
    }

    #[test]
    fn empty_transaction_prepares_the_original_state_as_a_noop() {
        let expected = empty_state();
        let mut budget = AssetLoadBudget::default();
        let transaction = WorkspaceStateTransaction::begin(Arc::clone(&expected), &mut budget)
            .expect("begin transaction");

        let prepared = transaction.commit(&mut budget).expect("commit no-op");
        let mut current = Arc::clone(&expected);

        assert!(!prepared.changed());
        assert_eq!(prepared.revision(), expected.revision());
        assert_eq!(
            prepared.install_into(&mut current),
            WorkspaceStateInstallOutcome::Unchanged
        );
        assert!(Arc::ptr_eq(&current, &expected));
    }

    #[test]
    fn late_joint_invariant_failure_does_not_prepare_partial_state() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join("missing.resource");
        fs::write(&path, b"missing").expect("write fixture");
        let expected = empty_state();
        let mut budget = AssetLoadBudget::default();
        let mut transaction = WorkspaceStateTransaction::begin(Arc::clone(&expected), &mut budget)
            .expect("begin transaction");
        transaction
            .register_descriptor(
                root_descriptor(&path, "missing.resource"),
                SourceFingerprint::from_bytes(SourceKind::StreamedResource, b"missing"),
                &mut budget,
            )
            .expect("register catalog candidate");

        assert!(matches!(
            transaction.commit(&mut budget),
            Err(WorkspaceStateError::SourceCardinalityMismatch {
                catalog: 1,
                store: 0
            })
        ));
        assert_eq!(expected.catalog().len(), 0);
        assert_eq!(expected.store().len(), 0);
    }

    #[test]
    fn failed_operation_aborts_all_later_transaction_work() {
        let expected = empty_state();
        let missing = SourceId::new(expected.workspace(), SourceKind::StreamedResource, 99)
            .expect("valid source");
        let mut budget = AssetLoadBudget::default();
        let mut transaction =
            WorkspaceStateTransaction::begin(expected, &mut budget).expect("begin transaction");

        assert!(transaction.remove_subtree(missing, &mut budget).is_err());
        assert!(matches!(
            transaction.commit(&mut budget),
            Err(WorkspaceStateError::TransactionAborted)
        ));
    }

    #[test]
    fn foreign_budget_content_aborts_the_transaction() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join("foreign.resource");
        fs::write(&path, b"foreign").expect("write fixture");
        let expected = empty_state();
        let mut owner_budget = AssetLoadBudget::default();
        let image = BudgetedSourceBytes::from_arc(Arc::from(&b"foreign"[..]), &mut owner_budget)
            .expect("budgeted source")
            .verify(SourceKind::StreamedResource);
        let fingerprint = image.fingerprint();
        let mut transaction_budget = AssetLoadBudget::default();
        let mut transaction = WorkspaceStateTransaction::begin(expected, &mut transaction_budget)
            .expect("begin transaction");
        let source = transaction
            .register_descriptor(
                root_descriptor(&path, "foreign.resource"),
                fingerprint,
                &mut transaction_budget,
            )
            .expect("register descriptor");
        let content = VerifiedSourceContent::from_budgeted(
            source,
            image,
            FrozenSourceParse::None,
            WorkspaceSourceFormatInspection::StreamedResource,
        );

        assert!(matches!(
            transaction.replace_verified_content(content, &mut transaction_budget),
            Err(WorkspaceStateError::Budget(BudgetError::DomainMismatch {
                resource: "verified source image"
            }))
        ));
        assert!(matches!(
            transaction.commit(&mut transaction_budget),
            Err(WorkspaceStateError::TransactionAborted)
        ));
    }

    #[test]
    fn physical_relocation_installs_even_when_revision_is_unchanged() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let first_path = directory.path().join("first.resource");
        let second_path = directory.path().join("second.resource");
        fs::write(&first_path, b"stable").expect("write first fixture");
        fs::write(&second_path, b"stable").expect("write second fixture");
        let canonical_second = fs::canonicalize(&second_path).expect("canonical second path");
        let mut budget = AssetLoadBudget::default();
        let empty = empty_state();
        let mut first = WorkspaceStateTransaction::begin(Arc::clone(&empty), &mut budget)
            .expect("begin first transaction");
        let source = first
            .register_tree(
                root_descriptor(&first_path, "stable.resource"),
                prepared_stream(b"stable", &mut budget),
                &mut budget,
            )
            .expect("register first binding");
        let first = first.commit(&mut budget).expect("prepare first state");
        let mut current = Arc::clone(&empty);
        assert_eq!(
            first.install_into(&mut current),
            WorkspaceStateInstallOutcome::Installed
        );
        let logical_revision = current.revision();

        let mut relocation = WorkspaceStateTransaction::begin(Arc::clone(&current), &mut budget)
            .expect("begin relocation");
        assert_eq!(
            relocation
                .remove_subtree(source, &mut budget)
                .expect("remove original binding"),
            vec![source]
        );
        let relocated = relocation
            .register_tree(
                root_descriptor(&second_path, "stable.resource"),
                prepared_stream(b"stable", &mut budget),
                &mut budget,
            )
            .expect("register relocated binding");
        assert_eq!(relocated, source);
        let relocation = relocation
            .commit(&mut budget)
            .expect("prepare relocated state");

        assert!(relocation.changed());
        assert_eq!(relocation.revision(), logical_revision);
        assert_eq!(
            relocation.install_into(&mut current),
            WorkspaceStateInstallOutcome::Installed
        );
        assert_eq!(
            relocation.install_into(&mut current),
            WorkspaceStateInstallOutcome::Unchanged
        );
        assert_eq!(current.revision(), logical_revision);
        assert_eq!(
            current
                .catalog()
                .physical_origin(source)
                .expect("relocated physical origin")
                .path(),
            canonical_second.as_path()
        );
    }
}
