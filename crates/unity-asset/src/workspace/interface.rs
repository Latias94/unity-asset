//! Authoritative workspace mutation boundary.

use std::collections::HashMap;
use std::fmt;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use unity_asset_binary::typetree::{
    CompositeTypeTreeRegistry, TypeTreeParseMode, TypeTreeParseOptions, TypeTreeRegistry,
};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedSourceBytes, SourceAlias, SourceId, SourceKind,
    WorkspaceId, WorkspaceRevision,
};

use super::adapter::binary::BinaryWorkspaceAdapter;
use super::snapshot::WorkspaceSnapshot;
use super::source_admission::{
    SourceAdmissionBatch, SourceAdmissionBatchAllocationError, SourceAdmissionBatchPhase,
    SourceAdmissionBatchPushError, SourceAdmissionDisposition, SourceAdmissionError,
    SourceAdmissionFailure, SourceAdmissionOperation, SourceAdmissionOperationLocation,
    SourceAdmissionOutcome, SourceAdmissionPolicy, SourceAdmissionRejection, SourceAdmissionReport,
};
use super::source_catalog::{
    CatalogError, PhysicalOrigin, RootAdmissionDecision, SourceDescriptor,
};
use super::source_loading::{prepare_root, read_owned_image, reserve_budgeted_vec, usize_to_u64};
use super::state::{
    PreparedSourceTree, PreparedWorkspaceState, WorkspaceState, WorkspaceStateInstallOutcome,
    WorkspaceStateTransaction,
};
use super::view::{WorkspaceAllocationUnit, WorkspaceError};

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

/// One explicit filesystem source load.
#[derive(Debug, Clone)]
pub struct SourceOpenRequest {
    path: PathBuf,
    alias: SourceAlias,
    kind_hint: Option<SourceKind>,
}

impl SourceOpenRequest {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, alias: SourceAlias) -> Self {
        Self {
            path: path.into(),
            alias,
            kind_hint: None,
        }
    }

    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        let path = path.into();
        let alias = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| WorkspaceError::InvalidSource {
                path: path.clone(),
                message: "the path has no portable UTF-8 file name; provide an explicit alias"
                    .to_owned(),
            })?
            .to_owned();
        Ok(Self::new(path, SourceAlias::new(alias)?))
    }

    #[must_use]
    pub fn with_kind_hint(mut self, kind: SourceKind) -> Self {
        self.kind_hint = Some(kind);
        self
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn alias(&self) -> &SourceAlias {
        &self.alias
    }

    #[must_use]
    pub const fn kind_hint(&self) -> Option<SourceKind> {
        self.kind_hint
    }

    pub(crate) fn retained_admission_bytes(&self) -> Result<u64, BudgetError> {
        self.path
            .capacity()
            .checked_add(self.alias.retained_owned_bytes())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "source admission operation metadata",
            })
    }
}

enum SourceImageInput {
    Path,
    Unaccounted(Arc<[u8]>),
    Budgeted(BudgetedSourceBytes),
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

    pub(super) fn install_prepared_state(
        &mut self,
        prepared: &PreparedWorkspaceState,
    ) -> WorkspaceStateInstallOutcome {
        prepared.install_into(&mut self.state)
    }

    pub fn load_path(
        &mut self,
        path: impl Into<PathBuf>,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceError> {
        self.load_source(SourceOpenRequest::from_path(path)?, budget)
    }

    /// Loads one coherent filesystem image into a new immutable workspace revision.
    ///
    /// The opened root is scanned twice and its physical identity is revalidated before
    /// publication. Both passes and the retained shared backing are charged to `budget`.
    pub fn load_source(
        &mut self,
        request: SourceOpenRequest,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceError> {
        let batch = single_admission_batch(SourceAdmissionOperation::LoadPath(request), budget)?;
        let report = self
            .admit_sources(batch, SourceAdmissionPolicy::Strict, budget)
            .map_err(source_admission_error_to_workspace)?;
        single_loaded_source(report)
    }

    /// Loads one caller-owned source image without reopening its physical path.
    ///
    /// The supplied bytes are the authoritative image retained by the resulting workspace
    /// revision. The path still establishes the durable physical origin used by publication and
    /// recovery checks, which will reject later on-disk divergence from this image's fingerprint.
    pub fn load_source_bytes(
        &mut self,
        request: SourceOpenRequest,
        image: Arc<[u8]>,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceError> {
        let batch = single_admission_batch(
            SourceAdmissionOperation::LoadBytes { request, image },
            budget,
        )?;
        let report = self
            .admit_sources(batch, SourceAdmissionPolicy::Strict, budget)
            .map_err(source_admission_error_to_workspace)?;
        single_loaded_source(report)
    }

    /// Loads a source backing whose shared allocation has already been charged to `budget`.
    ///
    /// This ownership-transfer entry point prevents cooperating scanners from charging the same
    /// immutable allocation again when the workspace retains it. Proofs minted by another load
    /// budget are rejected before workspace allocations are charged.
    pub fn load_budgeted_source_bytes(
        &mut self,
        request: SourceOpenRequest,
        image: BudgetedSourceBytes,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceError> {
        image.validate_budget(budget)?;
        let batch = single_admission_batch(
            SourceAdmissionOperation::LoadBudgetedBytes { request, image },
            budget,
        )?;
        let report = self
            .admit_sources(batch, SourceAdmissionPolicy::Strict, budget)
            .map_err(source_admission_error_to_workspace)?;
        single_loaded_source(report)
    }

    pub fn unload_source(
        &mut self,
        root: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), WorkspaceError> {
        let batch = single_admission_batch(SourceAdmissionOperation::Unload(root), budget)?;
        let report = self
            .admit_sources(batch, SourceAdmissionPolicy::Strict, budget)
            .map_err(source_admission_error_to_workspace)?;
        single_unloaded_source(report, root)
    }

    /// Prepares and applies an ordered source batch as one authoritative workspace transition.
    ///
    /// All load operations are fully parsed before the catalog/store candidate is created. A
    /// successful call installs at most one immutable state; any returned error leaves the
    /// authoritative state unchanged.
    pub fn admit_sources(
        &mut self,
        batch: SourceAdmissionBatch,
        policy: SourceAdmissionPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceAdmissionReport, SourceAdmissionError> {
        batch.validate_budget(budget).map_err(|error| {
            admission_batch_workspace_error(
                SourceAdmissionBatchPhase::Preparation,
                WorkspaceError::Budget(error),
            )
        })?;
        let base_revision = self.revision();
        let operations = batch.into_operations();
        if operations.is_empty() {
            return Ok(SourceAdmissionReport::new(
                policy,
                base_revision,
                base_revision,
                false,
                Vec::new(),
            ));
        }

        for (index, operation) in operations.iter().enumerate() {
            let SourceAdmissionOperation::LoadBudgetedBytes { image, .. } = operation else {
                continue;
            };
            let ordinal = admission_ordinal(index).map_err(|error| {
                admission_batch_workspace_error(SourceAdmissionBatchPhase::Preparation, error)
            })?;
            image.validate_budget(budget).map_err(|error| {
                SourceAdmissionError::operation(
                    ordinal,
                    None,
                    SourceAdmissionFailure::from(WorkspaceError::from(error)),
                )
            })?;
        }

        let load_count = operations
            .iter()
            .filter(|operation| !matches!(operation, SourceAdmissionOperation::Unload(_)))
            .count();
        let aliases = (load_count > 1)
            .then(|| {
                reserve_admission_index::<SourceAlias, (u64, SourceId)>(
                    load_count,
                    budget,
                    "source admission alias index",
                )
            })
            .transpose()
            .map_err(|error| {
                admission_batch_workspace_error(SourceAdmissionBatchPhase::Preparation, error)
            })?;
        let origins = (load_count > 1)
            .then(|| {
                reserve_admission_index::<PhysicalOrigin, (u64, SourceId)>(
                    load_count,
                    budget,
                    "source admission physical-origin index",
                )
            })
            .transpose()
            .map_err(|error| {
                admission_batch_workspace_error(SourceAdmissionBatchPhase::Preparation, error)
            })?;
        let mut prepared = reserve_budgeted_vec::<PreparedAdmission>(
            operations.len(),
            budget,
            "source admission preparation",
        )
        .map_err(|error| {
            admission_batch_workspace_error(SourceAdmissionBatchPhase::Preparation, error)
        })?;

        for (index, operation) in operations.into_iter().enumerate() {
            let ordinal = admission_ordinal(index).map_err(|error| {
                admission_batch_workspace_error(SourceAdmissionBatchPhase::Preparation, error)
            })?;
            let result = match operation {
                SourceAdmissionOperation::LoadPath(request) => {
                    self.prepare_admission_load(ordinal, request, SourceImageInput::Path, budget)
                }
                SourceAdmissionOperation::LoadBytes { request, image } => self
                    .prepare_admission_load(
                        ordinal,
                        request,
                        SourceImageInput::Unaccounted(image),
                        budget,
                    ),
                SourceAdmissionOperation::LoadBudgetedBytes { request, image } => self
                    .prepare_admission_load(
                        ordinal,
                        request,
                        SourceImageInput::Budgeted(image),
                        budget,
                    ),
                SourceAdmissionOperation::Unload(source) => {
                    Ok(PreparedAdmission::Unload { ordinal, source })
                }
            };
            match result {
                Ok(operation) => prepared.push(operation),
                Err(failure) if policy.tolerates(failure.category()) => {
                    prepared.push(PreparedAdmission::Rejected {
                        ordinal,
                        rejection: SourceAdmissionRejection::new(
                            failure.location,
                            *failure.failure,
                        ),
                    });
                }
                Err(failure) => {
                    return Err(SourceAdmissionError::operation(
                        ordinal,
                        failure.location,
                        *failure.failure,
                    ));
                }
            }
        }

        self.apply_prepared_admissions(prepared, aliases, origins, policy, base_revision, budget)
    }

    fn prepare_admission_load(
        &self,
        ordinal: u64,
        request: SourceOpenRequest,
        image: SourceImageInput,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedAdmission, AdmissionOperationFailure> {
        let SourceOpenRequest {
            path,
            alias,
            kind_hint,
        } = request;
        let absolute = match absolute_path(path, budget) {
            Ok(absolute) => absolute,
            Err((path, error)) => {
                return Err(AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::RequestPath(path),
                    SourceAdmissionFailure::from(*error),
                ));
            }
        };
        let origin = match PhysicalOrigin::from_existing_path_budgeted(&absolute, budget) {
            Ok(origin) => origin,
            Err(error) => {
                return Err(AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::RequestPath(absolute),
                    SourceAdmissionFailure::from(physical_origin_workspace_error(error)),
                ));
            }
        };
        drop(absolute);
        let image = match image {
            SourceImageInput::Path => match read_owned_image(&origin, budget) {
                Ok(image) => image,
                Err(error) => {
                    return Err(AdmissionOperationFailure::with_location(
                        SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                        SourceAdmissionFailure::from(error),
                    ));
                }
            },
            SourceImageInput::Unaccounted(image) => {
                match BudgetedSourceBytes::from_arc(image, budget).map_err(WorkspaceError::from) {
                    Ok(image) => image,
                    Err(error) => {
                        return Err(AdmissionOperationFailure::with_location(
                            SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                            SourceAdmissionFailure::from(error),
                        ));
                    }
                }
            }
            SourceImageInput::Budgeted(image) => {
                if let Err(error) = image.validate_budget(budget).map_err(WorkspaceError::from) {
                    return Err(AdmissionOperationFailure::with_location(
                        SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                        SourceAdmissionFailure::from(error),
                    ));
                }
                image
            }
        };
        let source = match prepare_root(
            origin.path(),
            kind_hint,
            image,
            &self.binary,
            self.source_registry.as_ref(),
            budget,
        ) {
            Ok(source) => source,
            Err(error) => {
                return Err(AdmissionOperationFailure::with_location(
                    SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                    SourceAdmissionFailure::from(error),
                ));
            }
        };
        Ok(PreparedAdmission::Load {
            ordinal,
            alias,
            origin,
            source: Box::new(source),
        })
    }

    fn apply_prepared_admissions(
        &mut self,
        prepared: Vec<PreparedAdmission>,
        mut aliases: Option<HashMap<SourceAlias, (u64, SourceId)>>,
        mut origins: Option<HashMap<PhysicalOrigin, (u64, SourceId)>>,
        policy: SourceAdmissionPolicy,
        base_revision: WorkspaceRevision,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceAdmissionReport, SourceAdmissionError> {
        let first_action = prepared.iter().find_map(PreparedAdmission::action_ordinal);
        let mut outcomes = reserve_budgeted_vec::<SourceAdmissionOutcome>(
            prepared.len(),
            budget,
            "source admission outcomes",
        )
        .map_err(|error| {
            admission_batch_workspace_error(SourceAdmissionBatchPhase::CandidateApplication, error)
        })?;

        let Some(_) = first_action else {
            for operation in prepared {
                let PreparedAdmission::Rejected { ordinal, rejection } = operation else {
                    return Err(admission_batch_protocol_error(
                        SourceAdmissionBatchPhase::CandidateApplication,
                        SourceAdmissionProtocolError::MissingAction,
                    ));
                };
                outcomes.push(SourceAdmissionOutcome::new(
                    ordinal,
                    SourceAdmissionDisposition::Rejected(rejection),
                ));
            }
            return Ok(SourceAdmissionReport::new(
                policy,
                base_revision,
                base_revision,
                false,
                outcomes,
            ));
        };
        let mut transaction = WorkspaceStateTransaction::begin(Arc::clone(&self.state), budget)
            .map_err(WorkspaceError::from)
            .map_err(|error| {
                admission_batch_workspace_error(
                    SourceAdmissionBatchPhase::CandidateApplication,
                    error,
                )
            })?;
        let mut candidate_changed = false;

        for operation in prepared {
            match operation {
                PreparedAdmission::Rejected { ordinal, rejection } => {
                    outcomes.push(SourceAdmissionOutcome::new(
                        ordinal,
                        SourceAdmissionDisposition::Rejected(rejection),
                    ));
                }
                PreparedAdmission::Unload { ordinal, source } => {
                    if source.workspace() != self.workspace_id() {
                        let error = unity_asset_core::ContractError::WorkspaceMismatch {
                            expected: self.workspace_id(),
                            actual: source.workspace(),
                        };
                        return Err(admission_workspace_error_at(
                            ordinal,
                            SourceAdmissionOperationLocation::Source(source),
                            error.into(),
                        ));
                    }
                    let is_root = transaction
                        .is_root(source)
                        .map_err(WorkspaceError::from)
                        .map_err(|error| {
                            admission_workspace_error_at(
                                ordinal,
                                SourceAdmissionOperationLocation::Source(source),
                                error,
                            )
                        })?;
                    if !is_root {
                        return Err(admission_workspace_error_at(
                            ordinal,
                            SourceAdmissionOperationLocation::Source(source),
                            WorkspaceError::NotRootSource(source),
                        ));
                    }
                    transaction
                        .remove_subtree(source, budget)
                        .map_err(WorkspaceError::from)
                        .map_err(|error| {
                            admission_workspace_error_at(
                                ordinal,
                                SourceAdmissionOperationLocation::Source(source),
                                error,
                            )
                        })?;
                    remove_admitted_source_indexes(&mut aliases, &mut origins, source);
                    candidate_changed = true;
                    outcomes.push(SourceAdmissionOutcome::new(
                        ordinal,
                        SourceAdmissionDisposition::Unloaded { source_id: source },
                    ));
                }
                PreparedAdmission::Load {
                    ordinal,
                    alias,
                    origin,
                    source,
                } => {
                    if let Some((first_operation, _)) =
                        aliases.as_ref().and_then(|aliases| aliases.get(&alias))
                    {
                        retain_admission_conflict(
                            policy,
                            ordinal,
                            SourceAdmissionOperationLocation::Alias(alias),
                            SourceAdmissionFailure::DuplicateAlias {
                                first_operation: *first_operation,
                            },
                            &mut outcomes,
                        )?;
                        continue;
                    }
                    if let Some((first_operation, _)) =
                        origins.as_ref().and_then(|origins| origins.get(&origin))
                    {
                        retain_admission_conflict(
                            policy,
                            ordinal,
                            SourceAdmissionOperationLocation::PhysicalOrigin(origin.into_path()),
                            SourceAdmissionFailure::DuplicatePhysicalOrigin {
                                first_operation: *first_operation,
                            },
                            &mut outcomes,
                        )?;
                        continue;
                    }
                    let fingerprint = source.fingerprint();
                    let decision = transaction
                        .root_admission_decision(&alias, &origin, fingerprint)
                        .map_err(WorkspaceError::from)
                        .map_err(|error| admission_workspace_error(ordinal, error))?;
                    match decision {
                        RootAdmissionDecision::Vacant => {}
                        RootAdmissionDecision::Unchanged(existing) => {
                            let (indexed_alias, indexed_origin) =
                                match prepare_admitted_source_index_keys(
                                    &alias,
                                    &origin,
                                    aliases.is_some(),
                                    origins.is_some(),
                                    budget,
                                ) {
                                    Ok(keys) => keys,
                                    Err(AdmissionIndexKeyError::Alias(error)) => {
                                        return Err(SourceAdmissionError::operation(
                                            ordinal,
                                            Some(SourceAdmissionOperationLocation::Alias(alias)),
                                            SourceAdmissionFailure::from(*error),
                                        ));
                                    }
                                    Err(AdmissionIndexKeyError::Origin(error)) => {
                                        return Err(SourceAdmissionError::operation(
                                            ordinal,
                                            Some(SourceAdmissionOperationLocation::PhysicalOrigin(
                                                origin.into_path(),
                                            )),
                                            SourceAdmissionFailure::from(*error),
                                        ));
                                    }
                                };
                            insert_admitted_source_index_keys(
                                &mut aliases,
                                &mut origins,
                                indexed_alias,
                                indexed_origin,
                                ordinal,
                                existing,
                            );
                            outcomes.push(SourceAdmissionOutcome::new(
                                ordinal,
                                SourceAdmissionDisposition::Unchanged {
                                    source_id: existing,
                                },
                            ));
                            continue;
                        }
                        RootAdmissionDecision::AliasConflict { existing } => {
                            retain_admission_conflict(
                                policy,
                                ordinal,
                                SourceAdmissionOperationLocation::Alias(alias),
                                SourceAdmissionFailure::AliasConflict {
                                    existing_source: existing,
                                },
                                &mut outcomes,
                            )?;
                            continue;
                        }
                        RootAdmissionDecision::PhysicalOriginConflict { existing } => {
                            retain_admission_conflict(
                                policy,
                                ordinal,
                                SourceAdmissionOperationLocation::PhysicalOrigin(
                                    origin.into_path(),
                                ),
                                SourceAdmissionFailure::PhysicalOriginConflict {
                                    existing_source: existing,
                                },
                                &mut outcomes,
                            )?;
                            continue;
                        }
                    }
                    let (indexed_alias, indexed_origin) = match prepare_admitted_source_index_keys(
                        &alias,
                        &origin,
                        aliases.is_some(),
                        origins.is_some(),
                        budget,
                    ) {
                        Ok(keys) => keys,
                        Err(AdmissionIndexKeyError::Alias(error)) => {
                            return Err(SourceAdmissionError::operation(
                                ordinal,
                                Some(SourceAdmissionOperationLocation::Alias(alias)),
                                SourceAdmissionFailure::from(*error),
                            ));
                        }
                        Err(AdmissionIndexKeyError::Origin(error)) => {
                            return Err(SourceAdmissionError::operation(
                                ordinal,
                                Some(SourceAdmissionOperationLocation::PhysicalOrigin(
                                    origin.into_path(),
                                )),
                                SourceAdmissionFailure::from(*error),
                            ));
                        }
                    };
                    let root_descriptor = SourceDescriptor::root(source.kind(), alias, origin);
                    let root = transaction
                        .register_tree(root_descriptor, *source, budget)
                        .map_err(WorkspaceError::from)
                        .map_err(|error| admission_workspace_error(ordinal, error))?;
                    insert_admitted_source_index_keys(
                        &mut aliases,
                        &mut origins,
                        indexed_alias,
                        indexed_origin,
                        ordinal,
                        root,
                    );
                    candidate_changed = true;
                    outcomes.push(SourceAdmissionOutcome::new(
                        ordinal,
                        SourceAdmissionDisposition::Loaded { source_id: root },
                    ));
                }
            }
        }

        if !candidate_changed {
            return Ok(SourceAdmissionReport::new(
                policy,
                base_revision,
                base_revision,
                false,
                outcomes,
            ));
        }

        let prepared_state = transaction
            .commit(budget)
            .map_err(WorkspaceError::from)
            .map_err(|error| {
                admission_batch_workspace_error(SourceAdmissionBatchPhase::Publication, error)
            })?;
        let revision = prepared_state.revision();
        let state_installed = match self.install_prepared_state(&prepared_state) {
            WorkspaceStateInstallOutcome::Installed => true,
            WorkspaceStateInstallOutcome::Unchanged => false,
            WorkspaceStateInstallOutcome::Stale => {
                return Err(admission_batch_workspace_error(
                    SourceAdmissionBatchPhase::Publication,
                    WorkspaceError::operation(
                        "source admission state installation",
                        std::io::Error::other(
                            "workspace changed before the prepared state could be installed",
                        ),
                    ),
                ));
            }
        };
        Ok(SourceAdmissionReport::new(
            policy,
            base_revision,
            revision,
            state_installed,
            outcomes,
        ))
    }
}

#[derive(Debug)]
struct AdmissionOperationFailure {
    location: Option<SourceAdmissionOperationLocation>,
    failure: Box<SourceAdmissionFailure>,
}

impl AdmissionOperationFailure {
    fn with_location(
        location: SourceAdmissionOperationLocation,
        failure: SourceAdmissionFailure,
    ) -> Self {
        Self {
            location: Some(location),
            failure: Box::new(failure),
        }
    }

    #[must_use]
    fn category(&self) -> super::source_admission::SourceAdmissionErrorCategory {
        self.failure.category()
    }
}

#[derive(Debug)]
enum AdmissionIndexKeyError {
    Alias(Box<WorkspaceError>),
    Origin(Box<WorkspaceError>),
}

#[derive(Debug)]
enum PreparedAdmission {
    Load {
        ordinal: u64,
        alias: SourceAlias,
        origin: PhysicalOrigin,
        source: Box<PreparedSourceTree>,
    },
    Unload {
        ordinal: u64,
        source: SourceId,
    },
    Rejected {
        ordinal: u64,
        rejection: SourceAdmissionRejection,
    },
}

impl PreparedAdmission {
    const fn action_ordinal(&self) -> Option<u64> {
        match self {
            Self::Load { ordinal, .. } | Self::Unload { ordinal, .. } => Some(*ordinal),
            Self::Rejected { .. } => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum SourceAdmissionProtocolError {
    #[error("a prepared batch without rejected-only operations has no action")]
    MissingAction,
    #[error("a single-operation admission returned {actual} outcomes")]
    UnexpectedOutcomeCount { actual: usize },
    #[error("a single load returned a non-load disposition")]
    UnexpectedLoadDisposition,
    #[error("a single unload returned a non-unload disposition")]
    UnexpectedUnloadDisposition,
}

fn single_admission_batch(
    operation: SourceAdmissionOperation,
    budget: &mut AssetLoadBudget,
) -> Result<SourceAdmissionBatch, WorkspaceError> {
    let mut batch = SourceAdmissionBatch::with_capacity(1, budget)
        .map_err(admission_batch_allocation_to_workspace)?;
    batch
        .try_push(operation, budget)
        .map_err(admission_batch_push_to_workspace)?;
    Ok(batch)
}

fn admission_batch_allocation_to_workspace(
    error: SourceAdmissionBatchAllocationError,
) -> WorkspaceError {
    match error {
        SourceAdmissionBatchAllocationError::Budget(error) => WorkspaceError::Budget(error),
        error @ SourceAdmissionBatchAllocationError::Allocation { .. } => {
            WorkspaceError::operation("source admission batch allocation", error)
        }
    }
}

fn admission_batch_push_to_workspace(error: SourceAdmissionBatchPushError) -> WorkspaceError {
    match error {
        SourceAdmissionBatchPushError::Budget(error) => WorkspaceError::Budget(error),
        SourceAdmissionBatchPushError::Capacity(error) => {
            WorkspaceError::operation("single source admission batch", error)
        }
    }
}

fn single_loaded_source(report: SourceAdmissionReport) -> Result<SourceId, WorkspaceError> {
    let actual = report.outcomes().len();
    if actual != 1 {
        return Err(WorkspaceError::operation(
            "single source admission",
            SourceAdmissionProtocolError::UnexpectedOutcomeCount { actual },
        ));
    }
    let mut outcomes = report.into_outcomes().into_iter();
    let Some(outcome) = outcomes.next() else {
        return Err(WorkspaceError::operation(
            "single source admission",
            SourceAdmissionProtocolError::UnexpectedOutcomeCount { actual: 0 },
        ));
    };
    match outcome.into_disposition() {
        SourceAdmissionDisposition::Loaded { source_id }
        | SourceAdmissionDisposition::Unchanged { source_id } => Ok(source_id),
        SourceAdmissionDisposition::Rejected(rejection) => {
            Err(admission_rejection_to_workspace(rejection))
        }
        SourceAdmissionDisposition::Unloaded { .. } => Err(WorkspaceError::operation(
            "single source admission",
            SourceAdmissionProtocolError::UnexpectedLoadDisposition,
        )),
    }
}

fn single_unloaded_source(
    report: SourceAdmissionReport,
    expected: SourceId,
) -> Result<(), WorkspaceError> {
    let actual = report.outcomes().len();
    if actual != 1 {
        return Err(WorkspaceError::operation(
            "single source admission",
            SourceAdmissionProtocolError::UnexpectedOutcomeCount { actual },
        ));
    }
    let mut outcomes = report.into_outcomes().into_iter();
    let Some(outcome) = outcomes.next() else {
        return Err(WorkspaceError::operation(
            "single source admission",
            SourceAdmissionProtocolError::UnexpectedOutcomeCount { actual: 0 },
        ));
    };
    match outcome.into_disposition() {
        SourceAdmissionDisposition::Unloaded { source_id } if source_id == expected => Ok(()),
        SourceAdmissionDisposition::Rejected(rejection) => {
            Err(admission_rejection_to_workspace(rejection))
        }
        SourceAdmissionDisposition::Loaded { .. }
        | SourceAdmissionDisposition::Unchanged { .. }
        | SourceAdmissionDisposition::Unloaded { .. } => Err(WorkspaceError::operation(
            "single source admission",
            SourceAdmissionProtocolError::UnexpectedUnloadDisposition,
        )),
    }
}

fn source_admission_error_to_workspace(error: SourceAdmissionError) -> WorkspaceError {
    let (site, failure) = error.into_parts();
    located_admission_failure_to_workspace(site.into_operation_location(), failure)
}

fn admission_rejection_to_workspace(rejection: SourceAdmissionRejection) -> WorkspaceError {
    let (location, failure) = rejection.into_parts();
    located_admission_failure_to_workspace(location, failure)
}

fn located_admission_failure_to_workspace(
    location: Option<SourceAdmissionOperationLocation>,
    failure: SourceAdmissionFailure,
) -> WorkspaceError {
    match failure {
        SourceAdmissionFailure::Workspace(error) => *error,
        failure @ SourceAdmissionFailure::DuplicateAlias { .. }
        | failure @ SourceAdmissionFailure::DuplicatePhysicalOrigin { .. }
        | failure @ SourceAdmissionFailure::AliasConflict { .. }
        | failure @ SourceAdmissionFailure::PhysicalOriginConflict { .. } => match location {
            Some(SourceAdmissionOperationLocation::Alias(alias)) => WorkspaceError::InvalidSource {
                path: PathBuf::from(alias.as_str()),
                message: failure.to_string(),
            },
            Some(SourceAdmissionOperationLocation::RequestPath(path))
            | Some(SourceAdmissionOperationLocation::PhysicalOrigin(path)) => {
                WorkspaceError::InvalidSource {
                    path,
                    message: failure.to_string(),
                }
            }
            Some(SourceAdmissionOperationLocation::Source(_)) | None => {
                WorkspaceError::operation("source admission", failure)
            }
        },
    }
}

fn admission_workspace_error(ordinal: u64, error: WorkspaceError) -> SourceAdmissionError {
    SourceAdmissionError::operation(ordinal, None, SourceAdmissionFailure::from(error))
}

fn retain_admission_conflict(
    policy: SourceAdmissionPolicy,
    ordinal: u64,
    location: SourceAdmissionOperationLocation,
    failure: SourceAdmissionFailure,
    outcomes: &mut Vec<SourceAdmissionOutcome>,
) -> Result<(), SourceAdmissionError> {
    if !policy.tolerates(failure.category()) {
        return Err(SourceAdmissionError::operation(
            ordinal,
            Some(location),
            failure,
        ));
    }
    outcomes.push(SourceAdmissionOutcome::new(
        ordinal,
        SourceAdmissionDisposition::Rejected(SourceAdmissionRejection::new(
            Some(location),
            failure,
        )),
    ));
    Ok(())
}

fn prepare_admitted_source_index_keys(
    alias: &SourceAlias,
    origin: &PhysicalOrigin,
    index_alias: bool,
    index_origin: bool,
    budget: &mut AssetLoadBudget,
) -> Result<(Option<SourceAlias>, Option<PhysicalOrigin>), AdmissionIndexKeyError> {
    let indexed_alias = index_alias
        .then(|| clone_admission_alias(alias, budget, "source admission alias key"))
        .transpose()
        .map_err(|error| AdmissionIndexKeyError::Alias(Box::new(error)))?;
    let indexed_origin = index_origin
        .then(|| clone_admission_origin(origin, budget, "source admission physical-origin key"))
        .transpose()
        .map_err(|error| AdmissionIndexKeyError::Origin(Box::new(error)))?;
    Ok((indexed_alias, indexed_origin))
}

fn insert_admitted_source_index_keys(
    aliases: &mut Option<HashMap<SourceAlias, (u64, SourceId)>>,
    origins: &mut Option<HashMap<PhysicalOrigin, (u64, SourceId)>>,
    indexed_alias: Option<SourceAlias>,
    indexed_origin: Option<PhysicalOrigin>,
    ordinal: u64,
    source: SourceId,
) {
    if let Some(indexed_alias) = indexed_alias {
        aliases
            .as_mut()
            .expect("an alias key requires an alias index")
            .insert(indexed_alias, (ordinal, source));
    }
    if let Some(indexed_origin) = indexed_origin {
        origins
            .as_mut()
            .expect("a physical-origin key requires a physical-origin index")
            .insert(indexed_origin, (ordinal, source));
    }
}

fn remove_admitted_source_indexes(
    aliases: &mut Option<HashMap<SourceAlias, (u64, SourceId)>>,
    origins: &mut Option<HashMap<PhysicalOrigin, (u64, SourceId)>>,
    source: SourceId,
) {
    if let Some(aliases) = aliases {
        aliases.retain(|_, (_, indexed_source)| *indexed_source != source);
    }
    if let Some(origins) = origins {
        origins.retain(|_, (_, indexed_source)| *indexed_source != source);
    }
}

fn admission_workspace_error_at(
    ordinal: u64,
    location: SourceAdmissionOperationLocation,
    error: WorkspaceError,
) -> SourceAdmissionError {
    SourceAdmissionError::operation(ordinal, Some(location), SourceAdmissionFailure::from(error))
}

fn admission_batch_workspace_error(
    phase: SourceAdmissionBatchPhase,
    error: WorkspaceError,
) -> SourceAdmissionError {
    SourceAdmissionError::batch(phase, SourceAdmissionFailure::from(error))
}

fn admission_batch_protocol_error(
    phase: SourceAdmissionBatchPhase,
    error: SourceAdmissionProtocolError,
) -> SourceAdmissionError {
    admission_batch_workspace_error(
        phase,
        WorkspaceError::operation("source admission protocol", error),
    )
}

fn physical_origin_workspace_error(error: CatalogError) -> WorkspaceError {
    match error {
        CatalogError::InvalidPhysicalOrigin(error) => {
            WorkspaceError::operation("physical-origin validation", error)
        }
        error => WorkspaceError::from(error),
    }
}

fn admission_ordinal(index: usize) -> Result<u64, WorkspaceError> {
    u64::try_from(index).map_err(|_| {
        WorkspaceError::Budget(BudgetError::ArithmeticOverflow {
            resource: "source_admission_ordinal",
        })
    })
}

fn reserve_admission_index<K, V>(
    count: usize,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<HashMap<K, V>, WorkspaceError>
where
    K: Eq + std::hash::Hash,
{
    // Cover capacity rounding, one control byte per bucket, and the trailing control group.
    let bucket_bytes = size_of::<(K, V)>()
        .checked_add(size_of::<u8>())
        .and_then(|bytes| bytes.checked_mul(count))
        .and_then(|bytes| bytes.checked_mul(2))
        .and_then(|bytes| bytes.checked_add(64))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bucket_bytes)?;
    let mut index = HashMap::new();
    index
        .try_reserve(count)
        .map_err(|error| WorkspaceError::Allocation {
            resource,
            requested: count,
            unit: WorkspaceAllocationUnit::Slots,
            message: error.to_string(),
        })?;
    budget.consume_bytes(bucket_bytes)?;
    Ok(index)
}

fn clone_admission_alias(
    alias: &SourceAlias,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<SourceAlias, WorkspaceError> {
    let length = alias.retained_clone_bytes();
    let bytes = u64::try_from(length).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let mut value = String::new();
    value
        .try_reserve_exact(length)
        .map_err(|error| WorkspaceError::Allocation {
            resource,
            requested: length,
            unit: WorkspaceAllocationUnit::Bytes,
            message: error.to_string(),
        })?;
    value.push_str(alias.as_str());
    let cloned = SourceAlias::new(value).map_err(WorkspaceError::from)?;
    let retained_bytes = u64::try_from(cloned.retained_owned_bytes())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(cloned)
}

fn clone_admission_origin(
    origin: &PhysicalOrigin,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<PhysicalOrigin, WorkspaceError> {
    let length = origin.path().as_os_str().len();
    let bytes = u64::try_from(length).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let cloned = origin
        .try_clone_for_index()
        .map_err(|error| WorkspaceError::Allocation {
            resource,
            requested: length,
            unit: WorkspaceAllocationUnit::Bytes,
            message: error.to_string(),
        })?;
    let retained_bytes = u64::try_from(cloned.retained_owned_bytes())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(cloned)
}

fn absolute_path(
    path: PathBuf,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, (PathBuf, Box<WorkspaceError>)> {
    const RESOURCE: &str = "source admission absolute path";

    if path.is_absolute() {
        return Ok(path);
    }
    let additional = match path.as_os_str().len().checked_add(1) {
        Some(additional) => additional,
        None => {
            return Err((
                path,
                Box::new(BudgetError::ArithmeticOverflow { resource: RESOURCE }.into()),
            ));
        }
    };
    let additional_bytes = match usize_to_u64(additional, RESOURCE) {
        Ok(additional_bytes) => additional_bytes,
        Err(error) => return Err((path, Box::new(error))),
    };
    if let Err(error) = budget.check_bytes(additional_bytes) {
        return Err((path, Box::new(error.into())));
    }
    let mut directory = match std::env::current_dir() {
        Ok(directory) => directory,
        Err(error) => {
            let error = WorkspaceError::io(&path, error);
            return Err((path, Box::new(error)));
        }
    };
    let minimum_capacity = match directory.as_os_str().len().checked_add(additional) {
        Some(minimum_capacity) => minimum_capacity.max(directory.capacity()),
        None => {
            return Err((
                path,
                Box::new(BudgetError::ArithmeticOverflow { resource: RESOURCE }.into()),
            ));
        }
    };
    let minimum_bytes = match usize_to_u64(minimum_capacity, RESOURCE) {
        Ok(minimum_bytes) => minimum_bytes,
        Err(error) => return Err((path, Box::new(error))),
    };
    if let Err(error) = budget.check_bytes(minimum_bytes) {
        return Err((path, Box::new(error.into())));
    }
    if let Err(error) = directory.as_mut_os_string().try_reserve_exact(additional) {
        return Err((
            path,
            Box::new(WorkspaceError::Allocation {
                resource: RESOURCE,
                requested: additional,
                unit: WorkspaceAllocationUnit::Bytes,
                message: error.to_string(),
            }),
        ));
    }
    let retained_bytes = match usize_to_u64(directory.capacity(), RESOURCE) {
        Ok(retained_bytes) => retained_bytes,
        Err(error) => return Err((path, Box::new(error))),
    };
    if let Err(error) = budget.check_bytes(retained_bytes) {
        return Err((path, Box::new(error.into())));
    }
    directory.push(&path);
    if let Err(error) = budget.consume_bytes(retained_bytes) {
        return Err((path, Box::new(error.into())));
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use unity_asset_core::AssetLoadLimits;

    #[test]
    fn absolute_request_path_transfers_without_a_second_budgeted_allocation() {
        let path = std::env::current_dir().expect("current directory");
        let capacity = path.capacity();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        })
        .expect("valid one-byte budget");

        let absolute = absolute_path(path, &mut budget).expect("transfer absolute path");

        assert!(absolute.is_absolute());
        assert_eq!(absolute.capacity(), capacity);
        assert_eq!(budget.usage(), Default::default());
    }

    #[test]
    fn relative_request_path_materialization_is_exact_budgeted() {
        let request = || PathBuf::from("relative").join("source.resource");
        let mut measured = AssetLoadBudget::default();
        let absolute =
            absolute_path(request(), &mut measured).expect("materialize measured absolute path");
        let measured_usage = measured.usage();
        assert!(absolute.is_absolute());
        assert!(absolute.ends_with(request()));
        assert!(measured_usage.bytes > 0);

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: measured_usage.bytes,
            ..AssetLoadLimits::default()
        })
        .expect("valid exact budget");
        absolute_path(request(), &mut exact).expect("materialize exact absolute path");
        assert_eq!(exact.usage().bytes, measured_usage.bytes);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: measured_usage.bytes - 1,
            ..AssetLoadLimits::default()
        })
        .expect("valid one-short budget");
        let (rejected, error) =
            absolute_path(request(), &mut one_short).expect_err("reject one-short budget");
        assert_eq!(rejected, request());
        assert!(matches!(
            *error,
            WorkspaceError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert_eq!(one_short.usage(), Default::default());
    }

    #[test]
    fn caller_owned_root_image_is_retained_without_reopening_the_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.resource");
        fs::write(&path, b"on disk").unwrap();
        let supplied: Arc<[u8]> = Arc::from(&b"supplied"[..]);
        let mut workspace = AssetWorkspace::new().unwrap();

        let source = workspace
            .load_source_bytes(
                SourceOpenRequest::from_path(&path).unwrap(),
                Arc::clone(&supplied),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(
            workspace
                .state()
                .store()
                .get(source)
                .unwrap()
                .image()
                .as_bytes(),
            supplied.as_ref()
        );
    }

    #[test]
    fn budgeted_root_image_transfers_without_duplicate_arc_accounting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.resource");
        fs::write(&path, b"four").unwrap();
        let image: Arc<[u8]> = Arc::from(&b"four"[..]);
        let request = || SourceOpenRequest::from_path(&path).unwrap();

        let mut direct_budget = AssetLoadBudget::default();
        AssetWorkspace::new()
            .unwrap()
            .load_source_bytes(request(), Arc::clone(&image), &mut direct_budget)
            .unwrap();

        let mut transferred_budget = AssetLoadBudget::default();
        let budgeted =
            BudgetedSourceBytes::from_arc(Arc::clone(&image), &mut transferred_budget).unwrap();
        AssetWorkspace::new()
            .unwrap()
            .load_budgeted_source_bytes(request(), budgeted, &mut transferred_budget)
            .unwrap();

        assert_eq!(transferred_budget.usage(), direct_budget.usage());
    }

    #[test]
    fn budgeted_root_image_rejects_a_different_budget_domain_before_accounting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.resource");
        fs::write(&path, b"four").unwrap();
        let mut source_budget = AssetLoadBudget::default();
        let image =
            BudgetedSourceBytes::from_arc(Arc::from(&b"four"[..]), &mut source_budget).unwrap();
        let mut load_budget = AssetLoadBudget::default();

        let error = AssetWorkspace::new()
            .unwrap()
            .load_budgeted_source_bytes(
                SourceOpenRequest::from_path(&path).unwrap(),
                image,
                &mut load_budget,
            )
            .unwrap_err();

        assert!(matches!(
            error,
            WorkspaceError::Budget(BudgetError::DomainMismatch {
                resource: "source bytes"
            })
        ));
        assert_eq!(load_budget.usage(), Default::default());
    }

    #[test]
    fn admitted_physical_origin_retention_is_budgeted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.resource");
        fs::write(&path, b"payload").unwrap();
        let run = |budget: &mut AssetLoadBudget| -> Result<_, WorkspaceError> {
            let workspace = AssetWorkspace::new().unwrap();
            let alias = SourceAlias::new("first.resource").unwrap();
            let origin = PhysicalOrigin::from_existing_path_budgeted(&path, budget)
                .map_err(physical_origin_workspace_error)?;
            let mut aliases = None;
            let mut origins = Some(reserve_admission_index::<PhysicalOrigin, (u64, SourceId)>(
                2,
                budget,
                "source admission physical-origin index",
            )?);
            let source =
                SourceId::new(workspace.workspace_id(), SourceKind::StreamedResource, 1).unwrap();
            let before_index = budget.usage().bytes;
            let (indexed_alias, indexed_origin) =
                prepare_admitted_source_index_keys(&alias, &origin, false, true, budget).map_err(
                    |error| match error {
                        AdmissionIndexKeyError::Alias(error)
                        | AdmissionIndexKeyError::Origin(error) => *error,
                    },
                )?;
            insert_admitted_source_index_keys(
                &mut aliases,
                &mut origins,
                indexed_alias,
                indexed_origin,
                0,
                source,
            );
            Ok((before_index, origins.unwrap(), origin, source))
        };

        let mut measured = AssetLoadBudget::default();
        let (before_index, origins, origin, source) = run(&mut measured).unwrap();
        assert!(measured.usage().bytes > before_index);
        assert_eq!(origins.get(&origin), Some(&(0, source)));
        let usage = measured.usage();

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        run(&mut exact).unwrap();
        assert_eq!(exact.usage().bytes, usage.bytes);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            run(&mut one_short),
            Err(WorkspaceError::Budget(_))
        ));
        assert!(one_short.usage().bytes < usage.bytes);
    }

    #[test]
    fn budgeted_yaml_root_does_not_charge_its_source_backing_twice() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.prefab");
        let encoded: Arc<[u8]> = Arc::from(b"root: value\n".as_slice());
        fs::write(&path, encoded.as_ref()).unwrap();
        let request = || SourceOpenRequest::from_path(&path).unwrap();

        let mut direct_budget = AssetLoadBudget::default();
        AssetWorkspace::new()
            .unwrap()
            .load_source_bytes(request(), Arc::clone(&encoded), &mut direct_budget)
            .unwrap();

        let mut transferred_budget = AssetLoadBudget::default();
        let image =
            BudgetedSourceBytes::from_arc(Arc::clone(&encoded), &mut transferred_budget).unwrap();
        AssetWorkspace::new()
            .unwrap()
            .load_budgeted_source_bytes(request(), image, &mut transferred_budget)
            .unwrap();

        assert_eq!(transferred_budget.usage(), direct_budget.usage());
    }

    #[test]
    fn workspace_binary_errors_preserve_the_standard_source_chain() {
        let root = WorkspaceError::from(
            unity_asset_binary::error::BinaryError::ResourceLimitExceeded("hard limit".to_owned()),
        );
        assert!(
            std::error::Error::source(&root)
                .and_then(|source| {
                    source.downcast_ref::<unity_asset_binary::error::BinaryError>()
                })
                .is_some_and(|source| matches!(source, unity_asset_binary::error::BinaryError::ResourceLimitExceeded(message) if message == "hard limit"))
        );
    }
}
