//! Workspace authority construction and immutable snapshot interface.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use unity_asset_binary::typetree::{
    CompositeTypeTreeRegistry, TypeTreeParseMode, TypeTreeParseOptions, TypeTreeRegistry,
};
use unity_asset_core::{AssetLoadBudget, WorkspaceId, WorkspaceRevision};

use super::adapter::binary::BinaryWorkspaceAdapter;
use super::snapshot::WorkspaceSnapshot;
use super::state::{PreparedWorkspaceState, WorkspaceState, WorkspaceStateInstallOutcome};
use super::view::WorkspaceError;

/// Immutable parsing policy shared by a workspace and every snapshot derived from it.
#[derive(Clone, Default)]
pub struct WorkspaceOptions {
    typetree: TypeTreeParseOptions,
    type_tree_registry: Option<Arc<dyn TypeTreeRegistry>>,
}

impl WorkspaceOptions {
    #[must_use]
    pub fn strict() -> Self {
        Self {
            typetree: TypeTreeParseOptions {
                mode: TypeTreeParseMode::Strict,
            },
            type_tree_registry: None,
        }
    }

    #[must_use]
    pub fn lenient() -> Self {
        Self::default()
    }

    /// Loads an immutable JSON/TPK registry under the caller's budget.
    ///
    /// Workspace loads deliberately reject arbitrary registry callbacks: snapshot state may only
    /// retain registries whose construction is budgeted and whose lookups are allocation-free.
    pub fn with_type_tree_registry_paths<P: AsRef<Path>>(
        mut self,
        paths: &[P],
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceError> {
        self.type_tree_registry = CompositeTypeTreeRegistry::from_paths(paths, budget)?;
        Ok(self)
    }

    #[must_use]
    pub const fn typetree_options(&self) -> TypeTreeParseOptions {
        self.typetree
    }
}

impl fmt::Debug for WorkspaceOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceOptions")
            .field("typetree_mode", &self.typetree.mode)
            .field("has_type_tree_registry", &self.type_tree_registry.is_some())
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct WorkspaceConfig {
    pub(crate) typetree: TypeTreeParseOptions,
}

impl fmt::Debug for WorkspaceConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceConfig")
            .field("typetree_mode", &self.typetree.mode)
            .finish_non_exhaustive()
    }
}

/// Mutable owner of one revisioned Unity source namespace.
pub struct AssetWorkspace {
    state: Arc<WorkspaceState>,
    config: Arc<WorkspaceConfig>,
    reference_store: Arc<crate::reference::ReferenceStore>,
    binary: BinaryWorkspaceAdapter,
    source_registry: Option<Arc<dyn TypeTreeRegistry>>,
}

impl fmt::Debug for AssetWorkspace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AssetWorkspace")
            .field("workspace_id", &self.workspace_id())
            .field("revision", &self.revision())
            .field("source_count", &self.state.store().len())
            .field("config", &self.config)
            .finish()
    }
}

impl AssetWorkspace {
    pub fn new() -> Result<Self, WorkspaceError> {
        Self::with_options(WorkspaceOptions::default())
    }

    /// Forks an isolated mutable candidate from the current immutable workspace state.
    ///
    /// The candidate initially shares revision-bound backing allocations, but every subsequent
    /// source admission installs state only in the returned workspace. This is the explicit seam
    /// for adapters that must finish an external publication before replacing their authoritative
    /// workspace; it is deliberately not a general [`Clone`] implementation.
    #[must_use]
    pub fn fork_candidate(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            config: Arc::clone(&self.config),
            reference_store: Arc::clone(&self.reference_store),
            binary: self.binary,
            source_registry: self.source_registry.clone(),
        }
    }

    pub fn with_options(options: WorkspaceOptions) -> Result<Self, WorkspaceError> {
        loop {
            if let Ok(workspace) = WorkspaceId::from_u128(rand::random()) {
                return Self::with_workspace_id(workspace, options);
            }
        }
    }

    /// Opens an empty workspace under a caller-persisted namespace identity.
    ///
    /// Workspace IDs are stable namespace keys, not authentication secrets.
    /// Recovery callers obtain the expected identity from
    /// [`crate::workspace::RecoveryOutcome::workspace_id`], then load source
    /// requests from their own trusted project configuration.
    pub fn with_workspace_id(
        workspace: WorkspaceId,
        options: WorkspaceOptions,
    ) -> Result<Self, WorkspaceError> {
        let state = WorkspaceState::empty(workspace, options.typetree.mode)
            .map_err(|source| WorkspaceError::operation("initialization", source))?;
        Ok(Self {
            state: Arc::new(state),
            config: Arc::new(WorkspaceConfig {
                typetree: options.typetree,
            }),
            reference_store: Arc::new(crate::reference::ReferenceStore::new()),
            binary: BinaryWorkspaceAdapter::new(),
            source_registry: options.type_tree_registry,
        })
    }

    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        self.state.workspace()
    }

    #[must_use]
    pub fn revision(&self) -> WorkspaceRevision {
        self.state.revision()
    }

    /// Returns the complete runtime source-to-physical-origin installation identity.
    #[must_use]
    pub fn installation_digest(&self) -> super::WorkspaceInstallationDigest {
        self.state.installation()
    }

    #[must_use]
    pub fn snapshot(&self) -> WorkspaceSnapshot {
        WorkspaceSnapshot::new(
            Arc::clone(&self.state),
            Arc::clone(&self.config),
            Arc::clone(&self.reference_store),
        )
    }

    #[must_use]
    pub(crate) const fn state(&self) -> &Arc<WorkspaceState> {
        &self.state
    }

    #[must_use]
    pub(crate) const fn binary_adapter(&self) -> &BinaryWorkspaceAdapter {
        &self.binary
    }

    pub(super) fn source_registry(&self) -> Option<&Arc<dyn TypeTreeRegistry>> {
        self.source_registry.as_ref()
    }

    pub(super) fn install_prepared_state(
        &mut self,
        prepared: &PreparedWorkspaceState,
    ) -> WorkspaceStateInstallOutcome {
        prepared.install_into(&mut self.state)
    }
}
