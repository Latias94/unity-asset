//! Authoritative workspace mutation boundary.

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use unity_asset_binary::asset::{ObjectInfo, SerializedFile, SerializedType};
use unity_asset_binary::typetree::{
    CompositeTypeTreeRegistry, TypeTree, TypeTreeNode, TypeTreeParseMode, TypeTreeParseOptions,
    TypeTreeRegistry, TypeTreeSchema, TypeTreeSemanticDigestError,
};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedSourceBytes, DigestBuildError, DigestV1, DigestV1Builder,
    SourceAlias, SourceId, SourceKind, UnityClass, UnityDocument, WorkspaceId, WorkspaceRevision,
    YamlFileId, arc_slice_allocation_bytes, arc_value_allocation_bytes,
};
use unity_asset_yaml::{BudgetedYamlError, YamlDocument, parse_prebudgeted_yaml_source};

use super::adapter::archive::{
    ArchiveLoadError, ArchiveMemberNameError, load_preflighted_zip_archive, preflight_zip_archive,
};
use super::adapter::binary::{
    BinaryAdapterAllocationUnit, BinaryAdapterError, BinaryContainerKind, BinaryMemberContent,
    BinaryPayload, BinaryWorkspaceAdapter,
};
use super::inspection::{
    AssetBundleSummary, SerializedFileSummary, WebFileSummary, WorkspaceSourceFormatInspection,
};
use super::snapshot::WorkspaceSnapshot;
use super::source_admission::{
    SourceAdmissionBatch, SourceAdmissionBatchAllocationError, SourceAdmissionBatchPhase,
    SourceAdmissionBatchPushError, SourceAdmissionDisposition, SourceAdmissionError,
    SourceAdmissionFailure, SourceAdmissionOperation, SourceAdmissionOperationLocation,
    SourceAdmissionOutcome, SourceAdmissionPolicy, SourceAdmissionRejection, SourceAdmissionReport,
};
use super::source_catalog::{
    CatalogError, PhysicalOrigin, RootAdmissionDecision, SourceDescriptor, open_verified_file,
    physical_file_identity, physical_file_identity_from_path,
};
use super::state::{
    FrozenSourceParse, PreparedSourceChild, PreparedSourceRelation, PreparedSourceTree,
    PreparedWorkspaceState, WorkspaceState, WorkspaceStateInstallOutcome,
    WorkspaceStateTransaction,
};
use super::view::{
    WorkspaceAllocationUnit, WorkspaceError, WorkspaceSourceContainer,
    WorkspaceSourceIdentityError, WorkspaceSourceMemberIdentityError,
};

const MAX_CONTAINER_DEPTH: u32 = 64;

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
            source,
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
                        .register_tree(root_descriptor, source, budget)
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
        source: PreparedSourceTree,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FrozenRegistryKey {
    Class(i32),
    Script { class_id: i32, script_id: [u8; 16] },
}

#[derive(Debug)]
struct FrozenRegistryEntry {
    key: FrozenRegistryKey,
    tree: Arc<TypeTree>,
    schema_digest: DigestV1,
}

#[derive(Debug)]
struct FrozenTypeTreeRegistry {
    entries: Vec<FrozenRegistryEntry>,
    digest: DigestV1,
}

impl TypeTreeRegistry for FrozenTypeTreeRegistry {
    fn resolve(&self, _unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        self.lookup(FrozenRegistryKey::Class(class_id))
    }

    fn semantic_digest(&self) -> Option<DigestV1> {
        Some(self.digest)
    }

    fn resolve_script(
        &self,
        _unity_version: &str,
        class_id: i32,
        script_id: [u8; 16],
    ) -> Option<Arc<TypeTree>> {
        self.lookup(FrozenRegistryKey::Script {
            class_id,
            script_id,
        })
    }
}

impl FrozenTypeTreeRegistry {
    fn lookup(&self, key: FrozenRegistryKey) -> Option<Arc<TypeTree>> {
        self.entries
            .binary_search_by_key(&key, |entry| entry.key)
            .ok()
            .map(|index| Arc::clone(&self.entries[index].tree))
    }
}

fn prepare_root(
    path: &Path,
    kind_hint: Option<SourceKind>,
    image: BudgetedSourceBytes,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
) -> Result<PreparedSourceTree, WorkspaceError> {
    image.validate_budget(budget)?;
    observe_container_depth(0, budget)?;
    match kind_hint {
        Some(SourceKind::Yaml) => prepare_yaml(image, budget, 0),
        Some(SourceKind::Archive) => prepare_archive(image, binary, source_registry, budget, 0),
        Some(SourceKind::StreamedResource) => Ok(prepared_raw(image)),
        Some(
            expected @ (SourceKind::SerializedFile | SourceKind::AssetBundle | SourceKind::WebFile),
        ) => {
            let payload = binary
                .parse_budgeted(&image, budget)
                .map_err(map_binary_adapter_error)?;
            let actual = binary_payload_kind(&payload);
            if actual != expected {
                return Err(WorkspaceError::InvalidSource {
                    path: path.to_path_buf(),
                    message: format!("expected {expected:?}, detected {actual:?}"),
                });
            }
            prepare_binary_payload(image, payload, binary, source_registry, budget, 0)
        }
        None if looks_like_zip(&image) || has_extension(path, &["zip", "apk"]) => {
            prepare_archive(image, binary, source_registry, budget, 0)
        }
        None if looks_like_yaml(&image) => prepare_yaml(image, budget, 0),
        None if has_yaml_extension(path) => {
            prepare_binary_or_yaml(image, binary, source_registry, budget, 0)
        }
        None if has_resource_extension(path) => Ok(prepared_raw(image)),
        None => {
            let payload = binary
                .parse_budgeted(&image, budget)
                .map_err(map_binary_adapter_error)?;
            prepare_binary_payload(image, payload, binary, source_registry, budget, 0)
        }
    }
}

fn prepare_yaml(
    image: BudgetedSourceBytes,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<PreparedSourceTree, WorkspaceError> {
    let mut scoped = budget.enter_depth(depth)?;
    let parsed = parse_prebudgeted_yaml_source(image, &mut scoped)
        .map_err(|error| map_yaml_error("YAML source parsing", error))?;
    let (image, document) = parsed.into_budgeted_parts(&scoped)?;
    drop(scoped);
    finish_prepared_yaml(image, document, budget)
}

fn finish_prepared_yaml(
    image: BudgetedSourceBytes,
    document: Arc<YamlDocument>,
    budget: &mut AssetLoadBudget,
) -> Result<PreparedSourceTree, WorkspaceError> {
    validate_yaml_identities(&document, budget)?;
    let document_count = usize_to_u64(document.entries().len(), "yaml_document_count")?;
    Ok(PreparedSourceTree::new(
        SourceKind::Yaml,
        image,
        FrozenSourceParse::Yaml(document),
        WorkspaceSourceFormatInspection::Yaml { document_count },
        Vec::new(),
    ))
}

fn prepare_binary_or_yaml(
    image: BudgetedSourceBytes,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<PreparedSourceTree, WorkspaceError> {
    let binary_result = {
        let mut scoped = budget.enter_depth(depth)?;
        binary.parse_budgeted(&image, &mut scoped)
    };
    match binary_result {
        Ok(payload) => {
            prepare_binary_payload(image, payload, binary, source_registry, budget, depth)
        }
        Err(BinaryAdapterError::FormatMismatch) => prepare_yaml(image, budget, depth),
        Err(source) => Err(map_binary_adapter_error(source)),
    }
}

fn prepare_archive(
    image: BudgetedSourceBytes,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<PreparedSourceTree, WorkspaceError> {
    image.validate_budget(budget)?;
    observe_container_depth(depth, budget)?;
    let plan = preflight_zip_archive(&image, budget)
        .map_err(|error| map_archive_error("ZIP archive preflight", error))?;
    let child_depth = plan
        .has_file_members()
        .then(|| next_container_depth(depth, budget))
        .transpose()?;
    let mut scoped = budget.enter_depth(depth)?;
    let archive_backing = image.clone_backing(&scoped)?;
    let members = load_preflighted_zip_archive(archive_backing, plan, &mut scoped)
        .map_err(|error| map_archive_error("ZIP archive loading", error))?;
    drop(scoped);
    let mut children = reserve_prepared_children(members.len(), budget)?;
    let mut previous_wire_ordinal = None;
    for member in members {
        if previous_wire_ordinal.is_some_and(|previous| previous >= member.wire_ordinal) {
            return Err(WorkspaceError::operation(
                "ZIP archive ordering",
                std::io::Error::other("archive members are not in strict wire order"),
            ));
        }
        previous_wire_ordinal = Some(member.wire_ordinal);
        let source = prepare_member(
            member.member_id.name(),
            member.bytes,
            binary,
            source_registry,
            budget,
            child_depth.ok_or_else(|| {
                WorkspaceError::operation(
                    "ZIP archive depth",
                    std::io::Error::other("archive preflight omitted a file member"),
                )
            })?,
            false,
        )?;
        children.push(PreparedSourceChild::new(
            PreparedSourceRelation::Archive,
            member.member_id,
            source,
        ));
    }
    let member_count = usize_to_u64(children.len(), "archive_member_count")?;
    Ok(PreparedSourceTree::new(
        SourceKind::Archive,
        image,
        FrozenSourceParse::None,
        WorkspaceSourceFormatInspection::Archive { member_count },
        children,
    ))
}

fn prepare_binary_payload(
    image: BudgetedSourceBytes,
    payload: BinaryPayload,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<PreparedSourceTree, WorkspaceError> {
    image.validate_budget(budget)?;
    observe_container_depth(depth, budget)?;
    let kind = binary_payload_kind(&payload);
    let relation = match kind {
        SourceKind::AssetBundle => Some(PreparedSourceRelation::Bundle),
        SourceKind::WebFile => Some(PreparedSourceRelation::WebFile),
        SourceKind::SerializedFile
        | SourceKind::Yaml
        | SourceKind::Archive
        | SourceKind::StreamedResource => None,
    };
    let has_members = relation.is_some() && binary.has_members(&payload);
    let child_depth = if has_members {
        Some(next_container_depth(depth, budget)?)
    } else {
        None
    };
    let members = if has_members {
        let mut scoped = budget.enter_depth(child_depth.ok_or_else(|| {
            WorkspaceError::operation(
                "binary member depth",
                std::io::Error::other("container source has no child depth"),
            )
        })?)?;
        binary
            .members(&payload, &mut scoped)
            .map_err(map_binary_adapter_error)?
    } else {
        Vec::new()
    };
    let (parsed, format) = match payload {
        BinaryPayload::SerializedFile(file) => {
            let file = freeze_serialized_registry(*file, source_registry, budget, depth)?;
            let summary = SerializedFileSummary::from_file(&file, budget)?;
            (
                FrozenSourceParse::Serialized(promote_value_to_arc(
                    file,
                    budget,
                    "workspace_serialized_file",
                )?),
                WorkspaceSourceFormatInspection::SerializedFile(summary),
            )
        }
        BinaryPayload::AssetBundle(bundle) => (
            FrozenSourceParse::None,
            WorkspaceSourceFormatInspection::AssetBundle(AssetBundleSummary::from_bundle(
                &bundle, budget,
            )?),
        ),
        BinaryPayload::WebFile(web_file) => (
            FrozenSourceParse::None,
            WorkspaceSourceFormatInspection::WebFile(WebFileSummary::from_webfile(
                &web_file, budget,
            )?),
        ),
    };
    let mut children = reserve_prepared_children(members.len(), budget)?;
    for member in members {
        let (_, identity, member_image, content) = member.into_parts();
        let source = match content {
            BinaryMemberContent::Parsed(payload) => prepare_binary_payload(
                member_image,
                payload,
                binary,
                source_registry,
                budget,
                child_depth.ok_or_else(|| {
                    WorkspaceError::operation(
                        "binary member depth",
                        std::io::Error::other("serialized source exposed a member"),
                    )
                })?,
            )?,
            BinaryMemberContent::RawResource => prepare_member(
                identity.name(),
                member_image,
                binary,
                source_registry,
                budget,
                child_depth.ok_or_else(|| {
                    WorkspaceError::operation(
                        "binary member depth",
                        std::io::Error::other("serialized source exposed a member"),
                    )
                })?,
                true,
            )?,
        };
        let relation = relation.ok_or_else(|| {
            WorkspaceError::operation(
                "binary member ownership",
                std::io::Error::other("serialized files cannot own container members"),
            )
        })?;
        children.push(PreparedSourceChild::new(relation, identity, source));
    }
    Ok(PreparedSourceTree::new(
        kind, image, parsed, format, children,
    ))
}

fn prepare_member(
    name: &str,
    image: BudgetedSourceBytes,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
    binary_already_rejected: bool,
) -> Result<PreparedSourceTree, WorkspaceError> {
    image.validate_budget(budget)?;
    observe_container_depth(depth, budget)?;
    let path = Path::new(name);
    if looks_like_zip(&image) || has_extension(path, &["zip", "apk"]) {
        return prepare_archive(image, binary, source_registry, budget, depth);
    }
    if looks_like_yaml(&image) {
        return prepare_yaml(image, budget, depth);
    }
    if has_yaml_extension(path) {
        return if binary_already_rejected {
            prepare_yaml(image, budget, depth)
        } else {
            prepare_binary_or_yaml(image, binary, source_registry, budget, depth)
        };
    }
    if has_resource_extension(path) || binary_already_rejected {
        return Ok(prepared_raw(image));
    }

    let binary_result = {
        let mut scoped = budget.enter_depth(depth)?;
        binary.parse_budgeted(&image, &mut scoped)
    };
    match binary_result {
        Ok(payload) => {
            prepare_binary_payload(image, payload, binary, source_registry, budget, depth)
        }
        Err(BinaryAdapterError::FormatMismatch) => Ok(prepared_raw(image)),
        Err(source) => Err(map_binary_adapter_error(source)),
    }
}

fn prepared_raw(image: BudgetedSourceBytes) -> PreparedSourceTree {
    PreparedSourceTree::new(
        SourceKind::StreamedResource,
        image,
        FrozenSourceParse::None,
        WorkspaceSourceFormatInspection::StreamedResource,
        Vec::new(),
    )
}

fn binary_payload_kind(payload: &BinaryPayload) -> SourceKind {
    match payload {
        BinaryPayload::SerializedFile(_) => SourceKind::SerializedFile,
        BinaryPayload::AssetBundle(_) => SourceKind::AssetBundle,
        BinaryPayload::WebFile(_) => SourceKind::WebFile,
    }
}

fn freeze_serialized_registry(
    mut file: SerializedFile,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<SerializedFile, WorkspaceError> {
    file = file.with_type_tree_registry(None);
    let Some(source_registry) = source_registry else {
        return Ok(file);
    };

    let object_count = file.objects().len();
    budget.consume_entries(usize_to_u64(object_count, "frozen_typetree_objects")?)?;
    let key_capacity = object_count
        .checked_mul(2)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "frozen_typetree_keys",
        })?;
    let mut keys = reserve_budgeted_vec::<FrozenRegistryKey>(
        key_capacity,
        budget,
        "frozen TypeTree lookup keys",
    )?;
    for object in file.objects() {
        let serialized_type = serialized_type_for_object(&file, object);
        if file.type_tree_enabled() && serialized_type.is_some_and(SerializedType::has_type_tree) {
            continue;
        }
        if let Some(serialized_type) = serialized_type
            && serialized_type.is_script_type()
            && serialized_type.script_id != [0; 16]
        {
            keys.push(FrozenRegistryKey::Script {
                class_id: serialized_type.class_id,
                script_id: serialized_type.script_id,
            });
        }
        keys.push(FrozenRegistryKey::Class(object.class_id()));
    }
    keys.sort_unstable();
    keys.dedup();
    budget.consume_entries(usize_to_u64(keys.len(), "frozen_typetree_keys")?)?;

    let mut entries =
        reserve_budgeted_vec::<FrozenRegistryEntry>(keys.len(), budget, "frozen TypeTree entries")?;
    for key in keys {
        let tree = match key {
            FrozenRegistryKey::Class(class_id) => {
                source_registry.resolve(&file.unity_version, class_id)
            }
            FrozenRegistryKey::Script {
                class_id,
                script_id,
            } => source_registry.resolve_script(&file.unity_version, class_id, script_id),
        };
        if let Some(tree) = tree {
            account_frozen_type_tree(&tree, budget, depth)?;
            let schema = TypeTreeSchema::compile(&tree, file.ref_types(), budget)?;
            let schema_digest = schema
                .semantic_digest_with_budget(budget)
                .map_err(|error| match error {
                    TypeTreeSemanticDigestError::Budget(error) => WorkspaceError::Budget(error),
                    TypeTreeSemanticDigestError::Digest(error) => {
                        WorkspaceError::operation("frozen TypeTree schema identity", error)
                    }
                })?;
            entries.push(FrozenRegistryEntry {
                key,
                tree,
                schema_digest,
            });
        }
    }
    if entries.is_empty() {
        return Ok(file);
    }

    let allocation = arc_value_allocation_bytes::<FrozenTypeTreeRegistry>().map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "frozen_typetree_registry",
        }
    })?;
    budget.consume_bytes(allocation)?;
    let digest = frozen_registry_digest(&entries)
        .map_err(|error| WorkspaceError::operation("frozen TypeTree registry identity", error))?;
    Ok(file.with_type_tree_registry(Some(Arc::new(FrozenTypeTreeRegistry { entries, digest }))))
}

fn frozen_registry_digest(entries: &[FrozenRegistryEntry]) -> Result<DigestV1, DigestBuildError> {
    const PREFIX: &[u8] = b"unity-asset:frozen-typetree-registry:v1\0";
    const COMMON_ENTRY_BYTES: u64 = 1 + 4 + DigestV1::BYTE_LEN as u64;

    let mut logical_length =
        u64::try_from(PREFIX.len()).map_err(|_| DigestBuildError::LengthOverflow)?;
    logical_length = logical_length
        .checked_add(8)
        .ok_or(DigestBuildError::LengthOverflow)?;
    for entry in entries {
        logical_length = logical_length
            .checked_add(COMMON_ENTRY_BYTES)
            .and_then(|length| {
                matches!(entry.key, FrozenRegistryKey::Script { .. })
                    .then(|| length.checked_add(16))
                    .unwrap_or(Some(length))
            })
            .ok_or(DigestBuildError::LengthOverflow)?;
    }

    let mut digest = DigestV1Builder::new(logical_length);
    digest.update(PREFIX)?;
    digest.update(
        &u64::try_from(entries.len())
            .map_err(|_| DigestBuildError::LengthOverflow)?
            .to_le_bytes(),
    )?;
    for entry in entries {
        match entry.key {
            FrozenRegistryKey::Class(class_id) => {
                digest.update(&[0])?;
                digest.update(&class_id.to_le_bytes())?;
            }
            FrozenRegistryKey::Script {
                class_id,
                script_id,
            } => {
                digest.update(&[1])?;
                digest.update(&class_id.to_le_bytes())?;
                digest.update(&script_id)?;
            }
        }
        digest.update(entry.schema_digest.as_bytes())?;
    }
    digest.finalize()
}

fn serialized_type_for_object<'file>(
    file: &'file SerializedFile,
    object: &ObjectInfo,
) -> Option<&'file SerializedType> {
    if let Some(index) = object.serialized_type_index() {
        return usize::try_from(index)
            .ok()
            .and_then(|index| file.types().get(index));
    }
    file.types()
        .iter()
        .find(|serialized_type| serialized_type.class_id == object.class_id())
}

fn account_frozen_type_tree(
    tree: &TypeTree,
    budget: &mut AssetLoadBudget,
    base_depth: u32,
) -> Result<(), WorkspaceError> {
    let mut scoped = budget.enter_depth(base_depth)?;
    if !tree.nodes.is_empty() {
        scoped.check_depth(0)?;
    }
    let top_level = size_of::<TypeTree>()
        .checked_add(
            tree.nodes
                .capacity()
                .checked_mul(size_of::<TypeTreeNode>())
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "frozen_typetree",
                })?,
        )
        .and_then(|bytes| bytes.checked_add(tree.string_buffer.capacity()))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "frozen_typetree",
        })?;
    scoped.consume_bytes(top_level)?;

    let mut stack = reserve_budgeted_vec::<(&TypeTreeNode, u32)>(
        tree.nodes.len(),
        &mut scoped,
        "frozen TypeTree traversal",
    )?;
    stack.extend(tree.nodes.iter().map(|node| (node, 0)));
    while let Some((node, depth)) = stack.pop() {
        scoped.consume_entries(1)?;
        scoped.observe_depth(depth)?;
        let retained = node
            .type_name
            .capacity()
            .checked_add(node.name.capacity())
            .and_then(|bytes| {
                node.children
                    .capacity()
                    .checked_mul(size_of::<TypeTreeNode>())
                    .and_then(|children| bytes.checked_add(children))
            })
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "frozen_typetree",
            })?;
        scoped.consume_bytes(retained)?;
        if !node.children.is_empty() {
            let child_depth = depth
                .checked_add(1)
                .ok_or(BudgetError::ArithmeticOverflow { resource: "depth" })?;
            scoped.check_depth(child_depth)?;
            let scratch = node
                .children
                .len()
                .checked_mul(size_of::<(&TypeTreeNode, u32)>())
                .and_then(|bytes| u64::try_from(bytes).ok())
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "frozen_typetree_traversal",
                })?;
            scoped.check_bytes(scratch)?;
            stack
                .try_reserve_exact(node.children.len())
                .map_err(|error| WorkspaceError::Allocation {
                    resource: "frozen TypeTree traversal",
                    requested: node.children.len(),
                    unit: WorkspaceAllocationUnit::Elements,
                    message: error.to_string(),
                })?;
            scoped.consume_bytes(scratch)?;
            stack.extend(node.children.iter().map(|child| (child, child_depth)));
        }
    }
    Ok(())
}

pub(super) fn validate_yaml_identities(
    document: &YamlDocument,
    budget: &mut AssetLoadBudget,
) -> Result<(), WorkspaceError> {
    budget.consume_entries(usize_to_u64(
        document.entries().len(),
        "yaml_identity_entries",
    )?)?;
    let mut file_ids = reserve_budgeted_vec::<YamlFileId>(
        document.entries().len(),
        budget,
        "YAML identity validation",
    )?;
    for (index, class) in document.entries().iter().enumerate() {
        if is_plain_yaml_document(index, class) {
            u32::try_from(index).map_err(|_| BudgetError::ArithmeticOverflow {
                resource: "yaml_document_ordinal",
            })?;
        } else {
            file_ids.push(YamlFileId::parse_canonical(class.anchor())?);
        }
    }
    file_ids.sort_unstable();
    if file_ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(WorkspaceError::InvalidSourceIdentity {
            source_kind: SourceKind::Yaml,
            reason: WorkspaceSourceIdentityError::DuplicateYamlFileId,
        });
    }
    Ok(())
}

fn is_plain_yaml_document(index: usize, class: &UnityClass) -> bool {
    class.class_id() == 0
        && class.class_name() == "YamlDocument"
        && class
            .anchor()
            .strip_prefix("doc_")
            .and_then(|ordinal| ordinal.parse::<usize>().ok())
            == Some(index)
}

fn observe_container_depth(depth: u32, budget: &mut AssetLoadBudget) -> Result<(), WorkspaceError> {
    if depth > MAX_CONTAINER_DEPTH {
        return Err(BudgetError::Exceeded {
            resource: "workspace_container_depth",
            limit: u64::from(MAX_CONTAINER_DEPTH),
            requested: u64::from(depth),
        }
        .into());
    }
    budget.observe_depth(depth)?;
    Ok(())
}

fn next_container_depth(depth: u32, budget: &mut AssetLoadBudget) -> Result<u32, WorkspaceError> {
    let next = depth
        .checked_add(1)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "depth" })?;
    observe_container_depth(next, budget)?;
    Ok(next)
}

fn reserve_prepared_children(
    count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<PreparedSourceChild>, WorkspaceError> {
    let bytes = count
        .checked_mul(size_of::<PreparedSourceChild>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "prepared_source_tree",
        })?;
    budget.check_bytes(bytes)?;
    let mut children = Vec::new();
    children
        .try_reserve_exact(count)
        .map_err(|error| WorkspaceError::Allocation {
            resource: "prepared source tree",
            requested: count,
            unit: WorkspaceAllocationUnit::Elements,
            message: error.to_string(),
        })?;
    budget.consume_bytes(bytes)?;
    Ok(children)
}

pub(super) fn map_yaml_error(operation: &'static str, error: BudgetedYamlError) -> WorkspaceError {
    match error {
        BudgetedYamlError::Budget(error) => WorkspaceError::Budget(error),
        BudgetedYamlError::AllocationFailed {
            context,
            requested,
            source,
        } => WorkspaceError::Allocation {
            resource: context,
            requested,
            unit: WorkspaceAllocationUnit::Bytes,
            message: source.to_string(),
        },
        BudgetedYamlError::IndexMapAllocationFailed {
            context,
            requested,
            source,
        } => WorkspaceError::Allocation {
            resource: context,
            requested,
            unit: WorkspaceAllocationUnit::Bytes,
            message: source.to_string(),
        },
        BudgetedYamlError::DepthExceeded { actual, limit } => {
            WorkspaceError::Budget(BudgetError::Exceeded {
                resource: "yaml_depth",
                limit: u64::from(limit),
                requested: u64::from(actual),
            })
        }
        error => WorkspaceError::operation(operation, error),
    }
}

fn map_archive_error(operation: &'static str, error: ArchiveLoadError) -> WorkspaceError {
    match error {
        ArchiveLoadError::Budget { source, .. } => WorkspaceError::Budget(source),
        ArchiveLoadError::Allocation {
            resource,
            requested,
            source,
        } => WorkspaceError::Allocation {
            resource,
            requested,
            unit: WorkspaceAllocationUnit::Bytes,
            message: source.to_string(),
        },
        ArchiveLoadError::ArithmeticOverflow { resource } => {
            WorkspaceError::Budget(BudgetError::ArithmeticOverflow { resource })
        }
        ArchiveLoadError::InvalidMemberName {
            wire_ordinal,
            reason,
        } => WorkspaceError::InvalidSourceMemberIdentity {
            container: WorkspaceSourceContainer::Archive,
            wire_ordinal,
            reason: match reason {
                ArchiveMemberNameError::Empty => WorkspaceSourceMemberIdentityError::Empty,
                ArchiveMemberNameError::TooLong => WorkspaceSourceMemberIdentityError::TooLong,
                ArchiveMemberNameError::UnstableEncoding => {
                    WorkspaceSourceMemberIdentityError::UnstableEncoding
                }
                ArchiveMemberNameError::Absolute => WorkspaceSourceMemberIdentityError::Absolute,
                ArchiveMemberNameError::Backslash => WorkspaceSourceMemberIdentityError::Backslash,
                ArchiveMemberNameError::ControlCharacter => {
                    WorkspaceSourceMemberIdentityError::ControlCharacter
                }
                ArchiveMemberNameError::TraversalComponent => {
                    WorkspaceSourceMemberIdentityError::TraversalComponent
                }
            },
        },
        ArchiveLoadError::MemberIdentity {
            wire_ordinal,
            source,
        } => WorkspaceError::InvalidSourceMemberIdentity {
            container: WorkspaceSourceContainer::Archive,
            wire_ordinal,
            reason: WorkspaceSourceMemberIdentityError::Contract(source),
        },
        error => WorkspaceError::operation(operation, error),
    }
}

pub(crate) fn map_binary_adapter_error(error: BinaryAdapterError) -> WorkspaceError {
    match error {
        BinaryAdapterError::Parse { source } => WorkspaceError::from(source),
        BinaryAdapterError::MemberBinary {
            container,
            wire_ordinal,
            source,
        } => map_binary_member_error(container, wire_ordinal, source),
        BinaryAdapterError::InvalidMemberIdentity {
            container,
            wire_ordinal,
            source,
        } => WorkspaceError::InvalidSourceMemberIdentity {
            container: map_binary_container(container),
            wire_ordinal,
            reason: WorkspaceSourceMemberIdentityError::Contract(source),
        },
        BinaryAdapterError::Budget(source) => WorkspaceError::Budget(source),
        BinaryAdapterError::Allocation {
            resource,
            requested,
            unit,
            source,
        } => WorkspaceError::Allocation {
            resource,
            requested,
            unit: match unit {
                BinaryAdapterAllocationUnit::Bytes => WorkspaceAllocationUnit::Bytes,
                BinaryAdapterAllocationUnit::Elements => WorkspaceAllocationUnit::Elements,
            },
            message: source.to_string(),
        },
        BinaryAdapterError::RetainedSizeOverflow { resource } => {
            WorkspaceError::Budget(BudgetError::ArithmeticOverflow { resource })
        }
        BinaryAdapterError::WireOrdinalOverflow => {
            WorkspaceError::Budget(BudgetError::ArithmeticOverflow {
                resource: "binary_member_ordinal",
            })
        }
        BinaryAdapterError::SameNameOccurrenceOverflow { .. } => {
            WorkspaceError::Budget(BudgetError::ArithmeticOverflow {
                resource: "binary_member_same_name_occurrence",
            })
        }
        BinaryAdapterError::FormatMismatch => {
            WorkspaceError::from(unity_asset_binary::error::BinaryError::invalid_format(
                "input is not a recognized Unity binary source",
            ))
        }
    }
}

fn map_binary_member_error(
    container: BinaryContainerKind,
    wire_ordinal: u64,
    source: unity_asset_binary::error::BinaryError,
) -> WorkspaceError {
    match source {
        source @ (unity_asset_binary::error::BinaryError::Budget(_)
        | unity_asset_binary::error::BinaryError::ObjectIdentity(_)) => {
            WorkspaceError::from(source)
        }
        source => WorkspaceError::BinaryMember {
            container: map_binary_container(container),
            wire_ordinal,
            source,
        },
    }
}

const fn map_binary_container(container: BinaryContainerKind) -> WorkspaceSourceContainer {
    match container {
        BinaryContainerKind::AssetBundle => WorkspaceSourceContainer::AssetBundle,
        BinaryContainerKind::WebFile => WorkspaceSourceContainer::WebFile,
    }
}

fn reserve_budgeted_vec<T>(
    count: usize,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<Vec<T>, WorkspaceError> {
    let bytes = count
        .checked_mul(size_of::<T>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|error| WorkspaceError::Allocation {
            resource,
            requested: count,
            unit: WorkspaceAllocationUnit::Elements,
            message: error.to_string(),
        })?;
    budget.consume_bytes(bytes)?;
    Ok(values)
}

fn consume_arc_allocation<T>(
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<(), WorkspaceError> {
    let bytes = arc_value_allocation_bytes::<T>()
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

pub(crate) fn promote_value_to_arc<T>(
    value: T,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<Arc<T>, WorkspaceError> {
    consume_arc_allocation::<T>(budget, resource)?;
    Ok(Arc::new(value))
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, WorkspaceError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource }.into())
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

fn read_owned_image(
    origin: &PhysicalOrigin,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedSourceBytes, WorkspaceError> {
    let path = origin.path();
    let mut file = open_verified_file(path).map_err(|error| WorkspaceError::io(path, error))?;
    let before = physical_file_identity(&file, path)?;
    let length = before.length();
    let length_usize = usize::try_from(length).map_err(|_| WorkspaceError::SourceTooLarge {
        path: path.to_path_buf(),
        length,
    })?;
    let retained_bytes = arc_slice_allocation_bytes::<u8>(length_usize).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "workspace_source_image",
        }
    })?;
    let planned_bytes = length
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(retained_bytes))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "workspace_source_image",
        })?;
    budget.check_bytes(planned_bytes)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length_usize)
        .map_err(|error| WorkspaceError::Allocation {
            resource: "workspace source image",
            requested: length_usize,
            unit: WorkspaceAllocationUnit::Bytes,
            message: error.to_string(),
        })?;
    budget.consume_bytes(length)?;
    bytes.resize(length_usize, 0);
    read_exact_stable(&mut file, &mut bytes, path)?;
    verify_stable_contents(&mut file, &bytes, path, budget)?;

    let after = physical_file_identity(&file, path)?;
    let current = physical_file_identity_from_path(path)?;
    if before != after || before != current {
        return Err(WorkspaceError::SourceChanged {
            path: path.to_path_buf(),
        });
    }
    BudgetedSourceBytes::from_vec(bytes, budget).map_err(WorkspaceError::from)
}

fn verify_stable_contents(
    reader: &mut (impl Read + Seek),
    expected: &[u8],
    path: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<(), WorkspaceError> {
    let length = usize_to_u64(expected.len(), "workspace_source_verification")?;
    budget.consume_bytes(length)?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| WorkspaceError::io(path, error))?;

    let mut verified = 0;
    let mut chunk = [0_u8; 64 * 1024];
    while verified < expected.len() {
        let count = chunk.len().min(expected.len() - verified);
        read_exact_stable(reader, &mut chunk[..count], path)?;
        if chunk[..count] != expected[verified..verified + count] {
            return Err(WorkspaceError::SourceChanged {
                path: path.to_path_buf(),
            });
        }
        verified += count;
    }

    let mut trailing = [0_u8; 1];
    if reader
        .read(&mut trailing)
        .map_err(|error| WorkspaceError::io(path, error))?
        != 0
    {
        return Err(WorkspaceError::SourceChanged {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn read_exact_stable(
    reader: &mut impl Read,
    bytes: &mut [u8],
    path: &Path,
) -> Result<(), WorkspaceError> {
    reader.read_exact(bytes).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            WorkspaceError::SourceChanged {
                path: path.to_path_buf(),
            }
        } else {
            WorkspaceError::io(path, error)
        }
    })
}

fn looks_like_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x06\x06")
}

fn looks_like_yaml(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let Some(text) = std::str::from_utf8(bytes).ok() else {
        return false;
    };
    let start = text.trim_start();
    start.starts_with("%YAML") || start.starts_with("--- !u!")
}

fn has_yaml_extension(path: &Path) -> bool {
    has_extension(
        path,
        &[
            "anim",
            "asset",
            "controller",
            "mat",
            "meta",
            "prefab",
            "unity",
        ],
    )
}

fn has_resource_extension(path: &Path) -> bool {
    has_extension(path, &["resource", "ress"])
}

fn has_extension(path: &Path, expected: &[&str]) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            expected
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use unity_asset_core::AssetLoadLimits;

    fn tree_with_child() -> TypeTree {
        let mut root = TypeTreeNode::new();
        root.children.push(TypeTreeNode::new());
        TypeTree {
            nodes: vec![root],
            ..TypeTree::default()
        }
    }

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
    fn frozen_leaf_root_uses_the_same_zero_based_depth_as_embedded_trees() {
        let tree = TypeTree {
            nodes: vec![TypeTreeNode::new()],
            ..TypeTree::default()
        };
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        account_frozen_type_tree(&tree, &mut budget, 1).unwrap();

        assert!(budget.usage().bytes > 0);
        assert_eq!(budget.usage().entries, 1);
        assert_eq!(budget.usage().max_observed_depth, 1);
    }

    #[test]
    fn frozen_tree_rejects_child_depth_before_child_traversal_scratch() {
        let tree = tree_with_child();
        let root = &tree.nodes[0];
        let expected_bytes = size_of::<TypeTree>()
            + tree.nodes.capacity() * size_of::<TypeTreeNode>()
            + tree.string_buffer.capacity()
            + size_of::<(&TypeTreeNode, u32)>()
            + root.type_name.capacity()
            + root.name.capacity()
            + root.children.capacity() * size_of::<TypeTreeNode>();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = account_frozen_type_tree(&tree, &mut budget, 1).unwrap_err();

        assert!(matches!(
            error,
            WorkspaceError::Budget(BudgetError::Exceeded {
                resource: "depth",
                limit: 1,
                requested: 2,
            })
        ));
        assert_eq!(budget.usage().bytes, u64::try_from(expected_bytes).unwrap());
        assert_eq!(budget.usage().entries, 1);
        assert_eq!(budget.usage().max_observed_depth, 1);
    }

    #[test]
    fn owned_root_image_accounts_read_verification_and_arc_backing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.resource");
        fs::write(&path, b"four").unwrap();
        let origin = PhysicalOrigin::from_existing_path(&path).unwrap();
        let arc_bytes = arc_slice_allocation_bytes::<u8>(4).unwrap();
        let exact_bytes = 8 + arc_bytes;

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = read_owned_image(&origin, &mut short).unwrap_err();
        assert!(matches!(
            error,
            WorkspaceError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == exact_bytes - 1 && requested == exact_bytes
        ));
        assert_eq!(short.usage().bytes, 0);

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let image = read_owned_image(&origin, &mut exact).unwrap();
        assert_eq!(image.as_ref(), b"four");
        assert_eq!(exact.usage().bytes, exact_bytes);
    }

    #[test]
    fn second_pass_rejects_same_length_content_change() {
        let path = Path::new("same-length.resource");
        let mut changed = std::io::Cursor::new(b"five".as_slice());
        let mut budget = AssetLoadBudget::default();

        let error = verify_stable_contents(&mut changed, b"four", path, &mut budget).unwrap_err();

        assert!(matches!(
            error,
            WorkspaceError::SourceChanged { path: changed_path } if changed_path == path
        ));
        assert_eq!(budget.usage().bytes, 4);
    }

    #[test]
    fn second_pass_classifies_truncation_as_source_change() {
        let path = Path::new("truncated.resource");
        let mut truncated = std::io::Cursor::new(b"thr".as_slice());

        let error = verify_stable_contents(
            &mut truncated,
            b"four",
            path,
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            WorkspaceError::SourceChanged { path: changed_path } if changed_path == path
        ));
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
    fn binary_adapter_resource_failures_keep_their_public_error_variants() {
        let memory = map_binary_adapter_error(BinaryAdapterError::Parse {
            source: unity_asset_binary::error::BinaryError::MemoryError(
                "allocation failed".to_owned(),
            ),
        });
        assert!(matches!(
            memory,
            WorkspaceError::Binary(unity_asset_binary::error::BinaryError::MemoryError(message))
                if message == "allocation failed"
        ));

        let hard_limit = map_binary_adapter_error(BinaryAdapterError::MemberBinary {
            container: BinaryContainerKind::WebFile,
            wire_ordinal: 7,
            source: unity_asset_binary::error::BinaryError::ResourceLimitExceeded(
                "member limit".to_owned(),
            ),
        });
        assert!(matches!(
            hard_limit,
            WorkspaceError::BinaryMember {
                container: WorkspaceSourceContainer::WebFile,
                wire_ordinal: 7,
                source: unity_asset_binary::error::BinaryError::ResourceLimitExceeded(message),
            } if message == "member limit"
        ));
    }

    #[test]
    fn allocation_mappers_preserve_bytes_elements_and_slots() {
        let reserve_error = || {
            Vec::<u8>::new()
                .try_reserve(usize::MAX)
                .expect_err("an impossible capacity must fail")
        };
        for (adapter_unit, expected) in [
            (
                BinaryAdapterAllocationUnit::Bytes,
                WorkspaceAllocationUnit::Bytes,
            ),
            (
                BinaryAdapterAllocationUnit::Elements,
                WorkspaceAllocationUnit::Elements,
            ),
        ] {
            let error = map_binary_adapter_error(BinaryAdapterError::Allocation {
                resource: "binary allocation",
                requested: 9,
                unit: adapter_unit,
                source: reserve_error(),
            });
            assert!(matches!(
                error,
                WorkspaceError::Allocation {
                    resource: "binary allocation",
                    requested: 9,
                    unit,
                    ..
                } if unit == expected
            ));
        }

        for (catalog_unit, expected) in [
            (
                crate::workspace::source_catalog::CatalogAllocationUnit::Bytes,
                WorkspaceAllocationUnit::Bytes,
            ),
            (
                crate::workspace::source_catalog::CatalogAllocationUnit::Elements,
                WorkspaceAllocationUnit::Elements,
            ),
            (
                crate::workspace::source_catalog::CatalogAllocationUnit::Slots,
                WorkspaceAllocationUnit::Slots,
            ),
        ] {
            let error = WorkspaceError::from(
                crate::workspace::source_catalog::CatalogError::AllocationFailed {
                    resource: "catalog allocation",
                    requested: 11,
                    unit: catalog_unit,
                    message: "allocation failed".to_owned(),
                },
            );
            assert!(matches!(
                error,
                WorkspaceError::Allocation {
                    resource: "catalog allocation",
                    requested: 11,
                    unit,
                    ..
                } if unit == expected
            ));
        }
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
