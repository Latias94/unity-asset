use std::cmp::Ordering;
use std::collections::TryReserveError;
use std::error::Error as StdError;
use std::ffi::OsStr;
use std::fmt;
use std::fs::Metadata;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedSourceBytes, DigestBuildError, DigestV1, DigestV1Builder,
    string_allocation_bytes, vec_allocation_bytes,
};
use unity_asset_search_core::SearchKind;
use unity_asset_search_protocol::ProjectId;

use super::candidate::{FileHint, ProjectSourcePath, ReadSource, ScanCandidate, SourceHints};
use super::diagnostic::{PathRejection, ScanDiagnostic, SourcePart};
use super::ledger::{ScanEntryKind, ScanLedger, ScanLimitError, ScanLimitResource};
use super::policy::{
    PolicyDecision, PolicyError, PolicyLimitResource, PolicyMatchBudget, SEARCH_IGNORE_V1_FILE,
    SearchIgnoreV1,
};
#[cfg(test)]
use crate::anchored_fs::OpenPolicy;
use crate::anchored_fs::{
    AnchoredFsError, DirectoryEntryHint, EntryKindHint, ReadDirectory, RegularFile,
    StableDirectoryIdentity, StableDirectoryObjectIdentity, StableFileIdentity,
};
use crate::path_semantics::{
    ProjectPathError, compare_portable_paths, is_portable_component,
    strip_prefix as strip_project_root,
};
#[cfg(windows)]
use crate::path_semantics::{
    windows_component_cmp as windows_path_component_cmp,
    windows_component_eq as windows_path_component_eq,
};
use crate::project_root::ProjectRootAuthority;
use crate::{IndexPaths, ProjectPathSet, SearchIndexOptions};

const SOURCE_IDENTITY_DOMAIN: &[u8] = b"unity-asset-search:source:v3";
const ASSET_COMPLETE: &[u8] = b"asset:complete";
const ASSET_UNAVAILABLE: &[u8] = b"asset:unavailable";
const META_PRESENT: &[u8] = b"meta:present";
const META_UNAVAILABLE: &[u8] = b"meta:unavailable";
const META_ABSENT: &[u8] = b"meta:absent";
const READ_BUFFER_BYTES: usize = 64 * 1024;
const READ_META_PATH_EXTRA_CAPACITY: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScanIntent {
    Full,
    Reconcile,
    ChangedPaths(ProjectPathSet),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanMode {
    Full,
    Reconcile,
    ChangedPaths,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScanReadLimits {
    pub(crate) max_asset_bytes: u64,
    pub(crate) max_retained_asset_bytes: u64,
    pub(crate) max_meta_bytes: u64,
}

impl Default for ScanReadLimits {
    fn default() -> Self {
        Self {
            max_asset_bytes: 2 * 1024 * 1024 * 1024,
            max_retained_asset_bytes: 64 * 1024 * 1024,
            max_meta_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ScanMetrics {
    pub(crate) discovered: u64,
    pub(crate) opened: u64,
    pub(crate) read_bytes: u64,
    pub(crate) unchanged: u64,
    pub(crate) deleted: u64,
}

impl ScanMetrics {
    pub(crate) fn merge(&mut self, other: &Self) {
        self.discovered = self.discovered.saturating_add(other.discovered);
        self.opened = self.opened.saturating_add(other.opened);
        self.read_bytes = self.read_bytes.saturating_add(other.read_bytes);
        self.unchanged = self.unchanged.saturating_add(other.unchanged);
        self.deleted = self.deleted.saturating_add(other.deleted);
    }
}

#[derive(Debug)]
pub(crate) struct ScanPlan {
    pub(crate) mode: ScanMode,
    pub(crate) present: Vec<ScanCandidate>,
    pub(crate) deleted: Vec<ProjectSourcePath>,
    pub(crate) diagnostics: Vec<ScanDiagnostic>,
    pub(crate) metrics: ScanMetrics,
    validation: ScanValidation,
    #[cfg(test)]
    traversal_usage: super::ledger::ScanLedgerUsage,
}

#[derive(Debug)]
pub(crate) struct ScanValidation {
    directory_proofs: Vec<DirectoryProof>,
    source_proofs: Vec<SourceProof>,
    absence_proofs: Vec<AbsenceProof>,
    policy_identity: Option<StableFileIdentity>,
}

impl ScanPlan {
    pub(crate) fn into_validation(self) -> ScanValidation {
        self.validation
    }

    pub(crate) fn record_source_proof(
        &mut self,
        proof: SourceProof,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<(), ScanError> {
        push_retained(
            &mut self.validation.source_proofs,
            proof,
            source_proof_backing_bytes,
            budget,
            "scan source proof list",
        )
    }
}

#[derive(Debug)]
pub(crate) enum ScanError {
    Budget(BudgetError),
    AllocationFailed {
        allocation: &'static str,
        requested: usize,
        source: TryReserveError,
    },
    Policy(PolicyError),
    PolicyRead {
        source: AnchoredFsError,
    },
    PolicyChangedDuringScan,
    TraversalRead {
        source: AnchoredFsError,
    },
    TraversalChangedDuringScan,
    SourceChangedDuringScan,
    TraversalLimitExceeded {
        resource: ScanLimitResource,
        observed_at_least: u64,
        limit: u64,
    },
    ChangedPathProjectMismatch {
        expected: ProjectId,
        actual: ProjectId,
    },
    ProjectPath(ProjectPathError),
}

impl ScanError {
    pub(crate) fn retryable(&self) -> bool {
        !matches!(
            self,
            Self::PolicyRead {
                source: AnchoredFsError::UnsupportedCaseSensitiveDirectory,
            } | Self::TraversalRead {
                source: AnchoredFsError::UnsupportedCaseSensitiveDirectory,
            }
        )
    }
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Budget(error) => fmt::Display::fmt(error, formatter),
            Self::AllocationFailed {
                allocation,
                requested,
                source,
            } => write!(
                formatter,
                "failed to reserve {requested} capacity units for {allocation}: {source}"
            ),
            Self::Policy(error) => fmt::Display::fmt(error, formatter),
            Self::PolicyRead { source } => write!(
                formatter,
                "failed to read project-root {SEARCH_IGNORE_V1_FILE}: {source}"
            ),
            Self::PolicyChangedDuringScan => write!(
                formatter,
                "project-root {SEARCH_IGNORE_V1_FILE} changed during the scan"
            ),
            Self::TraversalRead { source } => {
                write!(
                    formatter,
                    "failed to revalidate scanned directories: {source}"
                )
            }
            Self::TraversalChangedDuringScan => {
                formatter.write_str("the scanned directory namespace changed during the scan")
            }
            Self::SourceChangedDuringScan => {
                formatter.write_str("a scanned source changed during the scan")
            }
            Self::TraversalLimitExceeded {
                resource,
                observed_at_least,
                limit,
            } => write!(
                formatter,
                "scan {resource} limit exceeded: observed at least {observed_at_least}, limit \
                 {limit}"
            ),
            Self::ChangedPathProjectMismatch { expected, actual } => write!(
                formatter,
                "changed paths belong to project {actual}, but this scanner owns {expected}"
            ),
            Self::ProjectPath(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl StdError for ScanError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Budget(error) => Some(error),
            Self::AllocationFailed { source, .. } => Some(source),
            Self::Policy(error) => Some(error),
            Self::PolicyRead { source } => Some(source),
            Self::TraversalRead { source } => Some(source),
            Self::ProjectPath(error) => Some(error),
            Self::PolicyChangedDuringScan
            | Self::TraversalChangedDuringScan
            | Self::SourceChangedDuringScan
            | Self::TraversalLimitExceeded { .. }
            | Self::ChangedPathProjectMismatch { .. } => None,
        }
    }
}

impl From<BudgetError> for ScanError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<ScanLimitError> for ScanError {
    fn from(error: ScanLimitError) -> Self {
        Self::TraversalLimitExceeded {
            resource: error.resource,
            observed_at_least: error.observed_at_least,
            limit: error.limit,
        }
    }
}

impl From<PolicyError> for ScanError {
    fn from(error: PolicyError) -> Self {
        Self::Policy(error)
    }
}

#[derive(Debug)]
pub(crate) struct AcceptedReadSource {
    pub(crate) source: ReadSource,
    pub(crate) proof: SourceProof,
}

#[derive(Debug)]
pub(crate) struct ReadSourceOutcome {
    pub(crate) accepted: Option<AcceptedReadSource>,
    pub(crate) diagnostics: Vec<ScanDiagnostic>,
    pub(crate) metrics: ScanMetrics,
}

impl ReadSourceOutcome {
    fn rejected_without_diagnostic(metrics: ScanMetrics) -> Self {
        Self {
            accepted: None,
            diagnostics: Vec::new(),
            metrics,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ProjectScanner {
    project: ProjectRootAuthority,
    /// Project-root-relative scan roots. An empty path denotes the project root itself.
    scan_roots: Vec<PathBuf>,
    /// Project-root-relative private index namespace when it is nested inside the project.
    index_namespace_root: Option<PathBuf>,
    /// Handle-derived object identity of an existing nested private index namespace.
    index_namespace_identity: Option<StableDirectoryObjectIdentity>,
    options: SearchIndexOptions,
    limits: ScanReadLimits,
}

impl ProjectScanner {
    pub(crate) fn new(
        paths: &IndexPaths,
        options: SearchIndexOptions,
        limits: ScanReadLimits,
    ) -> Result<Self> {
        let project = paths.project_authority().clone();
        project
            .revalidate()
            .context("revalidate retained project authority before scanner construction")?;
        let project_root = project.root();
        let expected_project_id = paths.private_index_root().project_id();
        let actual_project_id = project.project_id();
        if actual_project_id != expected_project_id {
            return Err(anyhow!(
                "project root identity no longer matches its private index root: {}",
                project_root.display()
            ));
        }
        let mut scan_roots = paths
            .scan_roots()
            .iter()
            .map(|root| configured_relative_path(project_root, root))
            .collect::<Result<Vec<_>>>()?;
        for root in &scan_roots {
            if !root.as_os_str().is_empty() {
                project.directory().open_directory(root).with_context(|| {
                    format!(
                        "open scan root without following links: {}",
                        project_root.join(root).display()
                    )
                })?;
            }
        }
        normalize_relative_scan_roots(&mut scan_roots);
        if scan_roots.is_empty() {
            return Err(anyhow!("at least one scan root is required"));
        }

        let index_namespace_root = paths
            .index_namespace_exclusion()
            .map(|root| configured_relative_path(project_root, root))
            .transpose()?;
        let index_namespace_identity = match index_namespace_root.as_ref() {
            Some(relative) if relative.as_os_str().is_empty() => Some(
                project
                    .directory()
                    .object_identity()
                    .context("capture project-root index namespace identity")?,
            ),
            Some(relative) => match project.directory().open_directory(relative) {
                Ok(directory) => Some(directory.object_identity().with_context(|| {
                    format!(
                        "capture nested index namespace identity: {}",
                        paths.index_namespace_root().display()
                    )
                })?),
                Err(AnchoredFsError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "validate nested index namespace without following links: {}",
                            paths.index_namespace_root().display()
                        )
                    });
                }
            },
            None => None,
        };

        Ok(Self {
            project,
            scan_roots,
            index_namespace_root,
            index_namespace_identity,
            options,
            limits,
        })
    }

    fn project_root(&self) -> &Path {
        self.project.root()
    }

    fn read_root(&self) -> &ReadDirectory {
        self.project.directory()
    }

    fn project_source_path(
        &self,
        relative_path: String,
    ) -> std::result::Result<ProjectSourcePath, ScanError> {
        let path = self
            .project
            .path_space()
            .resolve(Path::new(&relative_path))
            .map_err(ScanError::ProjectPath)?
            .ok_or(ScanError::TraversalChangedDuringScan)?;
        Ok(ProjectSourcePath::from_project_path(&path, relative_path))
    }

    pub(crate) fn plan(
        &self,
        intent: ScanIntent,
        known_rel_paths: &[ProjectSourcePath],
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<ScanPlan, ScanError> {
        self.validate_project_root_binding()?;
        let mut diagnostics = Vec::new();
        let mut ledger = ScanLedger::new(self.options.scan_limits);
        let mut policy_match_budget =
            PolicyMatchBudget::new(self.options.scan_limits.max_policy_matches);
        let policy = self.load_policy(budget)?;
        let root_identity = self
            .read_root()
            .stable_identity()
            .map_err(directory_revalidation_error)?;
        let mut directory_proofs = Vec::new();
        push_retained(
            &mut directory_proofs,
            DirectoryProof {
                relative: PathBuf::new(),
                identity: root_identity,
            },
            directory_proof_backing_bytes,
            budget,
            "scan directory proof list",
        )?;
        let (mode, mut present, mut deleted, absence_proofs) = match intent {
            ScanIntent::Full => {
                let mut present = self.discover_full(
                    &policy.matcher,
                    &mut policy_match_budget,
                    &mut diagnostics,
                    &mut directory_proofs,
                    &mut ledger,
                    budget,
                )?;
                sort_and_dedup_candidates(&mut present);
                let mut deleted = Vec::new();
                append_missing_known_paths(known_rel_paths, &present, &mut deleted, budget)?;
                (ScanMode::Full, present, deleted, Vec::new())
            }
            ScanIntent::Reconcile => {
                let mut present = self.discover_full(
                    &policy.matcher,
                    &mut policy_match_budget,
                    &mut diagnostics,
                    &mut directory_proofs,
                    &mut ledger,
                    budget,
                )?;
                sort_and_dedup_candidates(&mut present);
                let mut deleted = Vec::new();
                append_missing_known_paths(known_rel_paths, &present, &mut deleted, budget)?;
                (ScanMode::Reconcile, present, deleted, Vec::new())
            }
            ScanIntent::ChangedPaths(paths) => {
                if paths.project_id() != self.project.project_id() {
                    return Err(ScanError::ChangedPathProjectMismatch {
                        expected: self.project.project_id(),
                        actual: paths.project_id(),
                    });
                }
                let changed = self.discover_changed(
                    paths,
                    known_rel_paths,
                    &policy.matcher,
                    &mut policy_match_budget,
                    &mut diagnostics,
                    &mut directory_proofs,
                    &mut ledger,
                    budget,
                )?;
                (
                    ScanMode::ChangedPaths,
                    changed.present,
                    changed.deleted,
                    changed.absence_proofs,
                )
            }
        };
        self.validate_policy_snapshot(policy.identity)?;
        self.validate_directory_proofs(&directory_proofs)?;
        sort_and_dedup_candidates(&mut present);
        diagnostics.sort_unstable_by(compare_scan_diagnostics);
        diagnostics.dedup();

        deleted.sort_unstable_by(|left, right| {
            left.identity()
                .cmp(&right.identity())
                .then_with(|| left.relative_path().cmp(right.relative_path()))
        });
        deleted.dedup_by(|left, right| left.identity() == right.identity());

        let metrics = ScanMetrics {
            discovered: saturating_usize_to_u64(present.len()),
            deleted: saturating_usize_to_u64(deleted.len()),
            ..ScanMetrics::default()
        };
        #[cfg(test)]
        let traversal_usage = ledger.usage();

        Ok(ScanPlan {
            mode,
            present,
            deleted,
            diagnostics,
            metrics,
            validation: ScanValidation {
                directory_proofs,
                source_proofs: Vec::new(),
                absence_proofs,
                policy_identity: policy.identity,
            },
            #[cfg(test)]
            traversal_usage,
        })
    }

    #[cfg(test)]
    pub(crate) fn validate_plan(&self, plan: &ScanPlan) -> std::result::Result<(), ScanError> {
        self.validate_scan(&plan.validation)
    }

    pub(crate) fn validate_scan(
        &self,
        validation: &ScanValidation,
    ) -> std::result::Result<(), ScanError> {
        self.validate_project_root_binding()?;
        self.validate_policy_snapshot(validation.policy_identity)?;
        self.validate_directory_proofs(&validation.directory_proofs)?;
        self.validate_source_proofs(&validation.source_proofs)?;
        self.validate_absence_proofs(&validation.absence_proofs)
    }

    pub(crate) fn validate_project_root_binding(&self) -> std::result::Result<(), ScanError> {
        self.project
            .validate_binding()
            .map_err(directory_revalidation_error)
    }

    pub(crate) fn read_source(
        &self,
        candidate: &ScanCandidate,
        previous_identity: Option<DigestV1>,
        budget: &mut AssetLoadBudget,
    ) -> ReadSourceOutcome {
        let mut metrics = ScanMetrics::default();
        let relative_path = candidate.relative_path();
        let asset_relative = Path::new(relative_path);
        if !is_supported_asset_path(asset_relative)
            || self.validate_relative_boundary(asset_relative).is_err()
        {
            return rejected_with_spec(
                ReadDiagnosticSpec::PathRejected {
                    path: DiagnosticPath::Joined {
                        root: self.project_root(),
                        relative: asset_relative,
                    },
                    reason: PathRejection::UnsupportedFileType,
                    part: SourcePart::Asset,
                },
                metrics,
                budget,
            );
        }
        let asset_file = match self.open_read_file(asset_relative, SourcePart::Asset) {
            Ok(file) => file,
            Err(failure) => {
                return rejected_read_failure(
                    &failure,
                    relative_path,
                    DiagnosticPath::Joined {
                        root: self.project_root(),
                        relative: asset_relative,
                    },
                    metrics,
                    budget,
                );
            }
        };
        let meta_relative = match retained_meta_relative_path(asset_relative, budget) {
            Ok(path) => path,
            Err(failure) => {
                return rejected_read_failure(
                    &failure,
                    relative_path,
                    DiagnosticPath::Joined {
                        root: self.project_root(),
                        relative: asset_relative,
                    },
                    metrics,
                    budget,
                );
            }
        };
        let meta_file = match self.read_root().open_regular(&meta_relative) {
            Ok(file) => Some(file),
            Err(AnchoredFsError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => {
                let failure = ReadFailure::Anchored {
                    part: SourcePart::Meta,
                    source,
                };
                return rejected_read_failure(
                    &failure,
                    relative_path,
                    DiagnosticPath::Joined {
                        root: self.project_root(),
                        relative: &meta_relative,
                    },
                    metrics,
                    budget,
                );
            }
        };

        let asset = match read_file_once(
            asset_file,
            SourcePart::Asset,
            self.limits.max_asset_bytes,
            self.limits.max_retained_asset_bytes,
            &mut metrics,
            budget,
        ) {
            Ok(asset) => asset,
            Err(failure) => {
                return rejected_read_failure(
                    &failure,
                    relative_path,
                    DiagnosticPath::Joined {
                        root: self.project_root(),
                        relative: asset_relative,
                    },
                    metrics,
                    budget,
                );
            }
        };

        let meta = match meta_file {
            Some(file) => match read_file_once(
                file,
                SourcePart::Meta,
                self.limits.max_meta_bytes,
                self.limits.max_meta_bytes,
                &mut metrics,
                budget,
            ) {
                Ok(meta) => Some(meta),
                Err(failure) => {
                    return rejected_read_failure(
                        &failure,
                        relative_path,
                        DiagnosticPath::Joined {
                            root: self.project_root(),
                            relative: &meta_relative,
                        },
                        metrics,
                        budget,
                    );
                }
            },
            None => None,
        };

        if let Err(failure) =
            self.revalidate_read_blob(asset_relative, SourcePart::Asset, &asset, &mut metrics)
        {
            return rejected_read_failure(
                &failure,
                relative_path,
                DiagnosticPath::Joined {
                    root: self.project_root(),
                    relative: asset_relative,
                },
                metrics,
                budget,
            );
        }
        if let Some(meta) = meta.as_ref() {
            if let Err(failure) =
                self.revalidate_read_blob(&meta_relative, SourcePart::Meta, meta, &mut metrics)
            {
                return rejected_read_failure(
                    &failure,
                    relative_path,
                    DiagnosticPath::Joined {
                        root: self.project_root(),
                        relative: &meta_relative,
                    },
                    metrics,
                    budget,
                );
            }
        } else if let Err(failure) = self.revalidate_absent_meta(&meta_relative, &mut metrics) {
            return rejected_read_failure(
                &failure,
                relative_path,
                DiagnosticPath::Joined {
                    root: self.project_root(),
                    relative: &meta_relative,
                },
                metrics,
                budget,
            );
        }

        let content_identity = match source_identity(
            SourceIdentityPart::from(&asset),
            meta.as_ref().map(SourceIdentityPart::from),
        ) {
            Ok(identity) => identity,
            Err(source) => {
                let failure = ReadFailure::Digest {
                    part: SourcePart::Asset,
                    source,
                };
                return rejected_read_failure(
                    &failure,
                    relative_path,
                    DiagnosticPath::Joined {
                        root: self.project_root(),
                        relative: asset_relative,
                    },
                    metrics,
                    budget,
                );
            }
        };

        if let Some(meta_bytes) = meta.as_ref().and_then(|meta| meta.bytes.as_ref())
            && let Err(error) = meta_bytes.validate_budget(budget)
        {
            return rejected_scan_error(error.into(), SourcePart::Meta, metrics, budget);
        }
        let (guid_value, malformed_guid) = meta
            .as_ref()
            .and_then(|meta| meta.bytes.as_deref())
            .map(guid_value_from_meta)
            .unwrap_or((None, false));
        let prepared = match prepare_read_source_backing(candidate, guid_value, budget) {
            Ok(prepared) => prepared,
            Err(error) => {
                return rejected_scan_error(error, SourcePart::Asset, metrics, budget);
            }
        };
        let diagnostic_specs = [
            asset
                .diagnostic
                .map(|note| read_note_spec(note, relative_path)),
            meta.as_ref()
                .and_then(|meta| meta.diagnostic)
                .map(|note| read_note_spec(note, relative_path)),
            malformed_guid.then_some(ReadDiagnosticSpec::MalformedGuid {
                rel_path: relative_path,
            }),
            (asset.digest.is_some() && asset.bytes.is_none()).then_some(
                ReadDiagnosticSpec::PayloadNotRetained {
                    rel_path: relative_path,
                    length: asset.hint.size,
                    retained_limit: self.limits.max_retained_asset_bytes,
                },
            ),
        ];
        let mut pending_diagnostics = PendingReadDiagnostics::default();
        for spec in diagnostic_specs.into_iter().flatten() {
            if let Err(error) = pending_diagnostics.push(spec, budget) {
                let part = spec.part();
                drop(prepared);
                drop(pending_diagnostics);
                return rejected_scan_error(error, part, metrics, budget);
            }
        }
        let diagnostics = pending_diagnostics.into_values();

        let identity_complete =
            asset.digest.is_some() && meta.as_ref().is_none_or(|meta| meta.digest.is_some());
        let unchanged = identity_complete && previous_identity == Some(content_identity);
        if unchanged {
            metrics.unchanged = metrics.unchanged.saturating_add(1);
        }

        let meta_hint = meta.as_ref().map(|meta| meta.hint);
        let meta_bytes = meta.as_ref().and_then(|meta| meta.bytes.clone());
        let abs_path = match prepared_joined_path(
            self.project_root(),
            asset_relative,
            budget,
            "read source absolute path",
        ) {
            Ok(path) => path,
            Err(error) => return rejected_scan_error(error, SourcePart::Asset, metrics, budget),
        };
        let source = ReadSource {
            coordinate: candidate.coordinate(),
            rel_path: prepared.rel_path,
            abs_path,
            name: prepared.name,
            kind: candidate.kind,
            guid: prepared.guid,
            bytes: asset.bytes.clone(),
            meta_bytes,
            length: asset.hint.size,
            content_identity,
            hints: SourceHints {
                asset: asset.hint,
                meta: meta_hint,
            },
            unchanged,
        };
        let source_proof = match prepared_source_proof(
            relative_path,
            &asset,
            meta.as_ref(),
            meta_relative,
            budget,
        ) {
            Ok(proof) => proof,
            Err(error) => return rejected_scan_error(error, SourcePart::Asset, metrics, budget),
        };

        ReadSourceOutcome {
            accepted: Some(AcceptedReadSource {
                source,
                proof: source_proof,
            }),
            diagnostics,
            metrics,
        }
    }

    fn load_policy(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<PolicySnapshot, ScanError> {
        let file = match self.read_root().open_regular(SEARCH_IGNORE_V1_FILE) {
            Ok(file) => file,
            Err(AnchoredFsError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(PolicySnapshot {
                    matcher: SearchIgnoreV1::compile(&[], self.options.ignore_limits, budget)?,
                    identity: None,
                });
            }
            Err(source) => return Err(ScanError::PolicyRead { source }),
        };
        let length = file.length();
        if length > self.options.ignore_limits.max_file_bytes {
            return Err(PolicyError::LimitExceeded {
                resource: PolicyLimitResource::FileBytes,
                observed_at_least: length,
                limit: self.options.ignore_limits.max_file_bytes,
            }
            .into());
        }
        let length = usize::try_from(length).map_err(|_| {
            ScanError::Budget(BudgetError::ArithmeticOverflow {
                resource: "SearchIgnoreV1 file bytes",
            })
        })?;
        budget.check_bytes(checked_usize_bytes(length)?)?;
        let mut source = Vec::new();
        source
            .try_reserve_exact(length)
            .map_err(|source| ScanError::AllocationFailed {
                allocation: "SearchIgnoreV1 source",
                requested: length,
                source,
            })?;
        budget.consume_bytes(checked_usize_bytes(source.capacity())?)?;
        source.resize(length, 0);
        file.read_exact_at(0, &mut source)
            .map_err(|source| ScanError::PolicyRead { source })?;
        let identity = file.stable_identity();
        self.validate_policy_snapshot(Some(identity))?;
        let matcher = SearchIgnoreV1::compile(&source, self.options.ignore_limits, budget)?;
        Ok(PolicySnapshot {
            matcher,
            identity: Some(identity),
        })
    }

    fn validate_policy_snapshot(
        &self,
        expected: Option<StableFileIdentity>,
    ) -> std::result::Result<(), ScanError> {
        match (
            expected,
            self.read_root().open_regular(SEARCH_IGNORE_V1_FILE),
        ) {
            (None, Err(AnchoredFsError::Io(error))) if error.kind() == io::ErrorKind::NotFound => {
                Ok(())
            }
            (Some(expected), Ok(file)) => {
                if file
                    .same_identity(expected)
                    .map_err(|source| ScanError::PolicyRead { source })?
                {
                    Ok(())
                } else {
                    Err(ScanError::PolicyChangedDuringScan)
                }
            }
            (None, Ok(_)) => Err(ScanError::PolicyChangedDuringScan),
            (Some(_), Err(AnchoredFsError::Io(error)))
                if error.kind() == io::ErrorKind::NotFound =>
            {
                Err(ScanError::PolicyChangedDuringScan)
            }
            (_, Err(source)) => Err(ScanError::PolicyRead { source }),
        }
    }

    fn validate_directory_proofs(
        &self,
        proofs: &[DirectoryProof],
    ) -> std::result::Result<(), ScanError> {
        for proof in proofs {
            let reopened = if proof.relative.as_os_str().is_empty() {
                self.project.reopen_bound()
            } else {
                self.read_root().open_directory(&proof.relative)
            }
            .map_err(directory_revalidation_error)?;
            reopened
                .ensure_identity(proof.identity)
                .map_err(directory_revalidation_error)?;
        }
        Ok(())
    }

    fn validate_source_proofs(&self, proofs: &[SourceProof]) -> std::result::Result<(), ScanError> {
        for proof in proofs {
            self.validate_source_file_proof(Path::new(&proof.relative), proof.asset)?;
            match proof.meta {
                Some(meta) => self.validate_source_file_proof(&proof.meta_relative, meta)?,
                None => self.validate_absent_source_file(&proof.meta_relative)?,
            }
        }
        Ok(())
    }

    fn validate_source_file_proof(
        &self,
        relative: &Path,
        expected: FileProof,
    ) -> std::result::Result<(), ScanError> {
        let reopened = match self.read_root().open_regular(relative) {
            Ok(file) => file,
            Err(source) => return Err(source_revalidation_error(source)),
        };
        let identity_matches = reopened
            .same_identity(expected.identity)
            .map_err(source_revalidation_error)?;
        if identity_matches {
            Ok(())
        } else {
            Err(ScanError::SourceChangedDuringScan)
        }
    }

    fn validate_absent_source_file(&self, relative: &Path) -> std::result::Result<(), ScanError> {
        match self.read_root().open_regular(relative) {
            Err(AnchoredFsError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(ScanError::SourceChangedDuringScan),
            Err(source) => Err(source_revalidation_error(source)),
        }
    }

    fn validate_absence_proofs(
        &self,
        proofs: &[AbsenceProof],
    ) -> std::result::Result<(), ScanError> {
        for proof in proofs {
            match self.open_relative_entry(Path::new(&proof.relative), EntryKindHint::Unknown) {
                Err(AnchoredFsError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => return Err(ScanError::SourceChangedDuringScan),
                Err(source) => return Err(source_revalidation_error(source)),
            }
        }
        Ok(())
    }

    fn discover_full(
        &self,
        policy: &SearchIgnoreV1,
        policy_match_budget: &mut PolicyMatchBudget,
        diagnostics: &mut Vec<ScanDiagnostic>,
        directory_proofs: &mut Vec<DirectoryProof>,
        ledger: &mut ScanLedger,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<Vec<ScanCandidate>, ScanError> {
        self.discover_scope(
            &DiscoveryScope::Full,
            policy,
            policy_match_budget,
            diagnostics,
            directory_proofs,
            ledger,
            budget,
        )
    }

    fn discover_changed(
        &self,
        paths: ProjectPathSet,
        known: &[ProjectSourcePath],
        policy: &SearchIgnoreV1,
        policy_match_budget: &mut PolicyMatchBudget,
        diagnostics: &mut Vec<ScanDiagnostic>,
        directory_proofs: &mut Vec<DirectoryProof>,
        ledger: &mut ScanLedger,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<ChangedDiscovery, ScanError> {
        let mut exact_files = Vec::new();
        let mut rescan_dirs = Vec::new();
        let mut requested = Vec::new();
        let mut rescan_prefixes = Vec::new();
        let mut absence_proofs = Vec::new();
        let mut policy_changed = false;

        for supplied in paths.into_paths() {
            let relative_input = supplied.as_relative_path();
            let normalized = match prepared_portable_relative_path(
                relative_input,
                budget,
                "changed relative path",
            )? {
                Ok(path) => path,
                Err(reason) => {
                    let diagnostic_path = prepared_joined_path(
                        self.project_root(),
                        relative_input,
                        budget,
                        "changed path diagnostic",
                    )?;
                    push_scan_diagnostic(
                        diagnostics,
                        ScanDiagnostic::PathRejected {
                            path: diagnostic_path,
                            reason,
                        },
                        ledger,
                        budget,
                    )?;
                    continue;
                }
            };
            if is_search_ignore_policy_path(&normalized) {
                policy_changed = true;
                continue;
            }
            let normalized = if is_meta_path(Path::new(&normalized)) {
                asset_path_from_meta(Path::new(&normalized))
                    .and_then(|path| path.to_str().map(str::to_owned))
                    .unwrap_or(normalized)
            } else {
                normalized
            };
            let relative = Path::new(&normalized);
            if let Err(reason) = self.validate_relative_boundary(relative) {
                let path = prepared_joined_path(
                    self.project_root(),
                    relative,
                    budget,
                    "changed path boundary diagnostic",
                )?;
                push_scan_diagnostic(
                    diagnostics,
                    ScanDiagnostic::PathRejected { path, reason },
                    ledger,
                    budget,
                )?;
                continue;
            }

            let mut delete_unconditionally = false;
            match self.open_relative_entry(relative, EntryKindHint::Unknown) {
                Ok(OpenedEntry::Directory(_)) => {
                    let path =
                        prepared_string_clone(&normalized, budget, "changed rescan directory")?;
                    push_retained(
                        &mut rescan_dirs,
                        path,
                        string_backing_bytes,
                        budget,
                        "changed rescan directory list",
                    )?;
                }
                Ok(OpenedEntry::File(_)) if is_supported_asset_path(relative) => {
                    let path = prepared_string_clone(&normalized, budget, "changed exact file")?;
                    push_retained(
                        &mut exact_files,
                        path,
                        string_backing_bytes,
                        budget,
                        "changed exact file list",
                    )?;
                }
                Ok(OpenedEntry::File(_)) | Ok(OpenedEntry::Other) => {
                    let path = prepared_joined_path(
                        self.project_root(),
                        relative,
                        budget,
                        "changed unsupported path diagnostic",
                    )?;
                    push_scan_diagnostic(
                        diagnostics,
                        ScanDiagnostic::PathRejected {
                            path,
                            reason: PathRejection::UnsupportedFileType,
                        },
                        ledger,
                        budget,
                    )?;
                }
                Err(AnchoredFsError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    delete_unconditionally = true;
                    let proof = AbsenceProof {
                        relative: prepared_string_clone(
                            &normalized,
                            budget,
                            "missing changed path proof",
                        )?,
                    };
                    push_retained(
                        &mut absence_proofs,
                        proof,
                        absence_proof_backing_bytes,
                        budget,
                        "missing changed path proof list",
                    )?;
                    if known_paths_have_strict_descendant(known, &normalized) {
                        let prefix =
                            prepared_string_clone(&normalized, budget, "missing rescan prefix")?;
                        push_retained(
                            &mut rescan_prefixes,
                            prefix,
                            string_backing_bytes,
                            budget,
                            "rescan prefix list",
                        )?;
                    }
                }
                Err(source) => {
                    push_anchored_diagnostic(
                        diagnostics,
                        self.project_root(),
                        relative,
                        source,
                        ledger,
                        budget,
                    )?;
                }
            }

            let path = if delete_unconditionally {
                match known_paths_get(known, &normalized) {
                    Some(known_path) => prepared_project_source_path_clone(
                        known_path,
                        budget,
                        "canonical deleted changed path",
                    )?,
                    None => self.project_source_path(normalized)?,
                }
            } else {
                self.project_source_path(normalized)?
            };
            let request = RequestedPath {
                path,
                delete_unconditionally,
            };
            push_retained(
                &mut requested,
                request,
                requested_path_backing_bytes,
                budget,
                "requested changed path list",
            )?;
        }

        if policy_changed {
            let mut present = self.discover_full(
                policy,
                policy_match_budget,
                diagnostics,
                directory_proofs,
                ledger,
                budget,
            )?;
            sort_and_dedup_candidates(&mut present);
            let mut deleted = Vec::new();
            append_missing_known_paths(known, &present, &mut deleted, budget)?;
            return Ok(ChangedDiscovery {
                present,
                deleted,
                absence_proofs: Vec::new(),
            });
        }

        sort_and_dedup_portable_paths(&mut exact_files);
        normalize_relative_prefixes(&mut rescan_dirs);
        for prefix in &rescan_dirs {
            let retained = prepared_string_clone(prefix, budget, "rescan prefix")?;
            push_retained(
                &mut rescan_prefixes,
                retained,
                string_backing_bytes,
                budget,
                "rescan prefix list",
            )?;
        }
        normalize_relative_prefixes(&mut rescan_prefixes);

        let scope = DiscoveryScope::Selected {
            exact_files: &exact_files,
            rescan_dirs: &rescan_dirs,
        };
        let mut present = self.discover_scope(
            &scope,
            policy,
            policy_match_budget,
            diagnostics,
            directory_proofs,
            ledger,
            budget,
        )?;
        sort_and_dedup_candidates(&mut present);
        let mut deleted = Vec::new();
        for request in requested {
            if request.delete_unconditionally
                || (known_paths_contain(known, request.path.relative_path())
                    && !contains_candidate(&present, &request.path))
            {
                push_retained(
                    &mut deleted,
                    request.path,
                    |_| Ok(0),
                    budget,
                    "known deletion list",
                )?;
            }
        }
        for prefix in &rescan_prefixes {
            for known_path in &known[known_path_descendant_range(known, prefix)] {
                if !contains_candidate(&present, known_path) {
                    let path = prepared_project_source_path_clone(
                        known_path,
                        budget,
                        "known deletion path",
                    )?;
                    push_retained(
                        &mut deleted,
                        path,
                        project_source_path_backing_bytes,
                        budget,
                        "known deletion list",
                    )?;
                }
            }
        }

        Ok(ChangedDiscovery {
            present,
            deleted,
            absence_proofs,
        })
    }

    fn observe_scan_root(
        &self,
        configured: &Path,
        diagnostics: &mut Vec<ScanDiagnostic>,
        directory_proofs: &mut Vec<DirectoryProof>,
        ledger: &mut ScanLedger,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<Option<ObservedScanRoot>, ScanError> {
        let mut handle = match self.project.reopen_bound() {
            Ok(handle) => handle,
            Err(source) => {
                push_anchored_diagnostic(
                    diagnostics,
                    self.project_root(),
                    configured,
                    source,
                    ledger,
                    budget,
                )?;
                return Ok(None);
            }
        };
        let mut relative = PathBuf::new();
        let mut normalized = String::new();

        for component in configured.components() {
            let Component::Normal(expected_name) = component else {
                return Err(ScanError::TraversalChangedDuringScan);
            };
            ledger.observe_kind(ScanEntryKind::Directory)?;
            let parent_identity = handle
                .stable_identity()
                .map_err(directory_revalidation_error)?;
            let child_depth = path_depth(&relative)?
                .checked_add(1)
                .ok_or(ScanError::Budget(BudgetError::ArithmeticOverflow {
                    resource: "scan path depth",
                }))?;
            let preflight =
                preflight_directory_entries(&handle, &normalized, child_depth, ledger, budget)?;
            let entries = collect_preflighted_directory_entries(
                &handle,
                &normalized,
                &preflight,
                parent_identity,
                budget,
            )?;
            if !relative.as_os_str().is_empty() {
                let proof = DirectoryProof {
                    relative: prepared_path_clone(
                        &relative,
                        budget,
                        "observed scan root ancestor proof",
                    )?,
                    identity: parent_identity,
                };
                push_retained(
                    directory_proofs,
                    proof,
                    directory_proof_backing_bytes,
                    budget,
                    "scan directory proof list",
                )?;
            }

            let mut observed_name = None;
            for entry in entries {
                if observed_component_matches(expected_name, entry.name()) {
                    if observed_name.is_some() {
                        return Err(ScanError::TraversalChangedDuringScan);
                    }
                    observed_name = Some(entry.into_name());
                }
            }
            let Some(observed_name) = observed_name else {
                push_anchored_diagnostic(
                    diagnostics,
                    self.project_root(),
                    configured,
                    AnchoredFsError::Io(io::Error::new(
                        io::ErrorKind::NotFound,
                        "configured scan root component is missing",
                    )),
                    ledger,
                    budget,
                )?;
                return Ok(None);
            };
            let observed_text = observed_name
                .to_str()
                .filter(|name| is_portable_component(name))
                .ok_or(ScanError::TraversalChangedDuringScan)?;
            let next_handle = match handle.open_directory(&observed_name) {
                Ok(handle) => handle,
                Err(source) => {
                    push_anchored_diagnostic(
                        diagnostics,
                        self.project_root(),
                        configured,
                        source,
                        ledger,
                        budget,
                    )?;
                    return Ok(None);
                }
            };
            let next_relative = prepared_relative_child_path(
                &relative,
                &observed_name,
                budget,
                "observed scan root relative path",
            )?;
            budget.consume_bytes(path_backing_bytes(&next_relative)?)?;
            let next_normalized = prepared_portable_child_path(
                &normalized,
                observed_text,
                budget,
                "observed scan root portable path",
            )?;
            budget.consume_bytes(string_backing_bytes(&next_normalized)?)?;
            handle = next_handle;
            relative = next_relative;
            normalized = next_normalized;
        }

        ledger.observe_kind(ScanEntryKind::Directory)?;
        let identity = handle
            .stable_identity()
            .map_err(directory_revalidation_error)?;
        Ok(Some(ObservedScanRoot {
            relative,
            normalized,
            handle,
            identity,
        }))
    }

    fn discover_scope(
        &self,
        scope: &DiscoveryScope<'_>,
        policy: &SearchIgnoreV1,
        policy_match_budget: &mut PolicyMatchBudget,
        diagnostics: &mut Vec<ScanDiagnostic>,
        directory_proofs: &mut Vec<DirectoryProof>,
        ledger: &mut ScanLedger,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<Vec<ScanCandidate>, ScanError> {
        let mut pending = Vec::new();
        for root in self.scan_roots.iter().rev() {
            let configured_normalized =
                relative_path_to_portable(root).ok_or(ScanError::TraversalChangedDuringScan)?;
            if !scope.should_visit_directory(&configured_normalized)
                || self.is_explicitly_excluded(root)
                || (!configured_normalized.is_empty()
                    && policy.decide(&configured_normalized, true, policy_match_budget)?
                        == PolicyDecision::ExcludeAndPrune)
            {
                continue;
            }
            let Some(observed) =
                self.observe_scan_root(root, diagnostics, directory_proofs, ledger, budget)?
            else {
                continue;
            };
            if self.is_index_directory(&observed.handle)? {
                continue;
            }
            let depth = path_depth(&observed.relative)?;
            ledger.observe_root_depth(depth)?;
            let directory = PendingDirectory {
                relative: observed.relative,
                normalized: observed.normalized,
                depth,
                identity: observed.identity,
            };
            push_retained(
                &mut pending,
                directory,
                |_| Ok(0),
                budget,
                "pending scan directory list",
            )?;
        }

        let mut present = Vec::new();
        while let Some(directory) = pending.pop() {
            let handle = if directory.relative.as_os_str().is_empty() {
                self.project.reopen_bound()
            } else {
                self.read_root().open_directory(&directory.relative)
            }
            .map_err(directory_revalidation_error)?;
            handle
                .ensure_identity(directory.identity)
                .map_err(directory_revalidation_error)?;
            let identity = directory.identity;
            let child_depth = directory.depth.checked_add(1).ok_or({
                ScanError::Budget(BudgetError::ArithmeticOverflow {
                    resource: "scan path depth",
                })
            })?;
            ledger.check_depth(child_depth)?;
            let preflight = preflight_directory_entries(
                &handle,
                &directory.normalized,
                child_depth,
                ledger,
                budget,
            )?;
            handle
                .ensure_identity(identity)
                .map_err(directory_revalidation_error)?;
            let handle = if directory.relative.as_os_str().is_empty() {
                self.project.reopen_bound()
            } else {
                self.read_root().open_directory(&directory.relative)
            }
            .map_err(directory_revalidation_error)?;
            handle
                .ensure_identity(identity)
                .map_err(directory_revalidation_error)?;
            let mut entries = collect_preflighted_directory_entries(
                &handle,
                &directory.normalized,
                &preflight,
                identity,
                budget,
            )?;
            sort_directory_entries(&mut entries);

            let mut children = Vec::new();
            for entry in entries {
                let hint = entry.kind();
                let name = entry.into_name();
                let Some(name_text) = name.to_str() else {
                    let path = prepared_relative_child_path(
                        &directory.relative,
                        &name,
                        budget,
                        "non-UTF8 directory entry",
                    )?;
                    push_scan_diagnostic(
                        diagnostics,
                        ScanDiagnostic::PathRejected {
                            path: prepared_joined_path(
                                self.project_root(),
                                &path,
                                budget,
                                "non-UTF8 path diagnostic",
                            )?,
                            reason: PathRejection::NonUtf8RelativePath,
                        },
                        ledger,
                        budget,
                    )?;
                    continue;
                };
                if name_text.is_empty() || matches!(name_text, "." | "..") {
                    continue;
                }
                if !is_portable_component(name_text) {
                    let path = prepared_relative_child_path(
                        &directory.relative,
                        &name,
                        budget,
                        "invalid portable directory entry",
                    )?;
                    push_scan_diagnostic(
                        diagnostics,
                        ScanDiagnostic::PathRejected {
                            path: prepared_joined_path(
                                self.project_root(),
                                &path,
                                budget,
                                "invalid portable path diagnostic",
                            )?,
                            reason: PathRejection::InvalidPath,
                        },
                        ledger,
                        budget,
                    )?;
                    continue;
                }
                let relative = prepared_relative_child_path(
                    &directory.relative,
                    &name,
                    budget,
                    "directory child relative path",
                )?;
                let normalized = prepared_portable_child_path(
                    &directory.normalized,
                    name_text,
                    budget,
                    "directory child portable path",
                )?;
                if self.is_explicitly_excluded(&relative) {
                    continue;
                }

                if !scope.should_visit_directory(&normalized) && !scope.includes_file(&normalized) {
                    continue;
                }

                match open_child_entry(&handle, &name, hint) {
                    Ok(OpenedEntry::Directory(handle)) => {
                        if self.is_index_directory(&handle)? {
                            continue;
                        }
                        ledger.observe_kind(ScanEntryKind::Directory)?;
                        if !scope.should_visit_directory(&normalized)
                            || policy.decide(&normalized, true, policy_match_budget)?
                                == PolicyDecision::ExcludeAndPrune
                        {
                            continue;
                        }
                        let child = PendingDirectory {
                            relative,
                            normalized,
                            depth: child_depth,
                            identity: handle
                                .stable_identity()
                                .map_err(directory_revalidation_error)?,
                        };
                        push_retained(
                            &mut children,
                            child,
                            pending_directory_backing_bytes,
                            budget,
                            "scan child directory list",
                        )?;
                    }
                    Ok(OpenedEntry::File(_file)) => {
                        ledger.observe_kind(ScanEntryKind::File)?;
                        if !scope.includes_file(&normalized)
                            || !is_supported_asset_path(&relative)
                            || policy.decide(&normalized, false, policy_match_budget)?
                                != PolicyDecision::Include
                        {
                            continue;
                        }
                        let Some(stem) = relative.file_stem().and_then(OsStr::to_str) else {
                            let path = prepared_joined_path(
                                self.project_root(),
                                &relative,
                                budget,
                                "candidate stem diagnostic",
                            )?;
                            push_scan_diagnostic(
                                diagnostics,
                                ScanDiagnostic::PathRejected {
                                    path,
                                    reason: PathRejection::NonUtf8RelativePath,
                                },
                                ledger,
                                budget,
                            )?;
                            continue;
                        };
                        let project_path = self.project_source_path(normalized)?;
                        let candidate = ScanCandidate::new(
                            project_path,
                            prepared_string_clone(stem, budget, "scan candidate name")?,
                            classify_kind(&relative),
                        );
                        push_retained(
                            &mut present,
                            candidate,
                            candidate_backing_bytes,
                            budget,
                            "scan candidate list",
                        )?;
                    }
                    Ok(OpenedEntry::Other) => {}
                    Err(source) => push_anchored_diagnostic(
                        diagnostics,
                        self.project_root(),
                        &relative,
                        source,
                        ledger,
                        budget,
                    )?,
                }
            }
            children.sort_unstable_by(|left, right| left.normalized.cmp(&right.normalized));
            for child in children.into_iter().rev() {
                push_retained(
                    &mut pending,
                    child,
                    |_| Ok(0),
                    budget,
                    "pending scan directory list",
                )?;
            }
            handle
                .ensure_identity(identity)
                .map_err(directory_revalidation_error)?;
            push_retained(
                directory_proofs,
                DirectoryProof {
                    relative: directory.relative,
                    identity,
                },
                |_| Ok(0),
                budget,
                "scan directory proof list",
            )?;
        }
        Ok(present)
    }

    fn open_relative_entry(
        &self,
        relative: &Path,
        hint: EntryKindHint,
    ) -> std::result::Result<OpenedEntry, AnchoredFsError> {
        if relative.as_os_str().is_empty() {
            return self.project.reopen_bound().map(OpenedEntry::Directory);
        }
        let directory_first = !matches!(hint, EntryKindHint::RegularFile);
        if directory_first {
            match self.read_root().open_directory(relative) {
                Ok(directory) => return Ok(OpenedEntry::Directory(directory)),
                Err(error) if !is_anchored_type_mismatch(&error) => return Err(error),
                Err(_) => {}
            }
            match self.read_root().open_regular(relative) {
                Ok(file) => Ok(OpenedEntry::File(file)),
                Err(error) if is_anchored_type_mismatch(&error) => Ok(OpenedEntry::Other),
                Err(error) => Err(error),
            }
        } else {
            match self.read_root().open_regular(relative) {
                Ok(file) => return Ok(OpenedEntry::File(file)),
                Err(error) if !is_anchored_type_mismatch(&error) => return Err(error),
                Err(_) => {}
            }
            match self.read_root().open_directory(relative) {
                Ok(directory) => Ok(OpenedEntry::Directory(directory)),
                Err(error) if is_anchored_type_mismatch(&error) => Ok(OpenedEntry::Other),
                Err(error) => Err(error),
            }
        }
    }

    fn validate_relative_boundary(
        &self,
        relative: &Path,
    ) -> std::result::Result<(), PathRejection> {
        if !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(PathRejection::InvalidPath);
        }
        if !self
            .scan_roots
            .iter()
            .any(|root| root.as_os_str().is_empty() || relative_path_starts_with(relative, root))
        {
            return Err(PathRejection::OutsideScanRoots);
        }
        if self
            .index_namespace_root
            .as_ref()
            .is_some_and(|root| relative_path_starts_with(relative, root))
        {
            return Err(PathRejection::InsideIndexRoot);
        }
        if is_explicit_unity_exclusion(relative) {
            return Err(PathRejection::Excluded);
        }
        Ok(())
    }

    fn is_explicitly_excluded(&self, relative: &Path) -> bool {
        self.index_namespace_root
            .as_ref()
            .is_some_and(|root| relative_path_starts_with(relative, root))
            || is_explicit_unity_exclusion(relative)
    }

    fn is_index_directory(
        &self,
        directory: &ReadDirectory,
    ) -> std::result::Result<bool, ScanError> {
        self.index_namespace_identity.map_or(Ok(false), |identity| {
            directory
                .same_object(identity)
                .map_err(directory_revalidation_error)
        })
    }

    fn open_read_file(
        &self,
        relative: &Path,
        part: SourcePart,
    ) -> std::result::Result<RegularFile, ReadFailure> {
        self.read_root()
            .open_regular(relative)
            .map_err(|source| ReadFailure::Anchored { part, source })
    }

    fn revalidate_read_blob(
        &self,
        relative: &Path,
        part: SourcePart,
        blob: &ReadBlob,
        metrics: &mut ScanMetrics,
    ) -> std::result::Result<(), ReadFailure> {
        let mut reopened = match self.read_root().open_regular(relative) {
            Ok(file) => file,
            Err(AnchoredFsError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ReadFailure::Changed {
                    part,
                    before: Some(blob.snapshot.hint),
                    after: None,
                });
            }
            Err(source) => return Err(ReadFailure::Anchored { part, source }),
        };
        metrics.opened = metrics.opened.saturating_add(1);
        let metadata = reopened
            .file_mut()
            .metadata()
            .map_err(|source| ReadFailure::Io { part, source })?;
        let current = FileSnapshot::from_metadata(&metadata);
        let same_identity = reopened
            .same_identity(blob.identity)
            .map_err(|source| ReadFailure::Anchored { part, source })?;
        if !same_identity || current != blob.snapshot {
            return Err(ReadFailure::Changed {
                part,
                before: Some(blob.snapshot.hint),
                after: Some(current.hint),
            });
        }
        Ok(())
    }

    fn revalidate_absent_meta(
        &self,
        relative: &Path,
        metrics: &mut ScanMetrics,
    ) -> std::result::Result<(), ReadFailure> {
        let mut file = match self.read_root().open_regular(relative) {
            Err(AnchoredFsError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(source) => {
                return Err(ReadFailure::Anchored {
                    part: SourcePart::Meta,
                    source,
                });
            }
            Ok(file) => file,
        };
        metrics.opened = metrics.opened.saturating_add(1);
        let metadata = file
            .file_mut()
            .metadata()
            .map_err(|source| ReadFailure::Io {
                part: SourcePart::Meta,
                source,
            })?;
        Err(ReadFailure::Changed {
            part: SourcePart::Meta,
            before: None,
            after: Some(file_hint(&metadata)),
        })
    }
}

#[derive(Debug)]
struct PolicySnapshot {
    matcher: SearchIgnoreV1,
    identity: Option<StableFileIdentity>,
}

#[derive(Debug, Clone, Copy)]
enum DiscoveryScope<'targets> {
    Full,
    Selected {
        exact_files: &'targets [String],
        rescan_dirs: &'targets [String],
    },
}

impl DiscoveryScope<'_> {
    fn should_visit_directory(self, path: &str) -> bool {
        match self {
            Self::Full => true,
            Self::Selected {
                exact_files,
                rescan_dirs,
            } => {
                sorted_paths_have_descendant(exact_files, path)
                    || sorted_paths_have_descendant(rescan_dirs, path)
                    || sorted_prefixes_contain(rescan_dirs, path)
            }
        }
    }

    fn includes_file(self, path: &str) -> bool {
        match self {
            Self::Full => true,
            Self::Selected {
                exact_files,
                rescan_dirs,
            } => {
                sorted_paths_contain(exact_files, path)
                    || sorted_prefixes_contain(rescan_dirs, path)
            }
        }
    }
}

#[derive(Debug)]
struct ObservedScanRoot {
    relative: PathBuf,
    normalized: String,
    handle: ReadDirectory,
    identity: StableDirectoryIdentity,
}

#[derive(Debug)]
struct PendingDirectory {
    relative: PathBuf,
    normalized: String,
    depth: u32,
    identity: StableDirectoryIdentity,
}

#[derive(Debug, Clone, Copy)]
struct DirectoryEntryPreflight {
    count: u64,
    path_bytes: u64,
    retained_name_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectoryProof {
    relative: PathBuf,
    identity: StableDirectoryIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileProof {
    identity: StableFileIdentity,
}

#[derive(Debug)]
pub(crate) struct SourceProof {
    relative: String,
    asset: FileProof,
    meta_relative: PathBuf,
    meta: Option<FileProof>,
}

#[derive(Debug)]
struct AbsenceProof {
    relative: String,
}

#[derive(Debug)]
enum OpenedEntry {
    Directory(ReadDirectory),
    File(RegularFile),
    Other,
}

#[derive(Debug)]
struct ChangedDiscovery {
    present: Vec<ScanCandidate>,
    deleted: Vec<ProjectSourcePath>,
    absence_proofs: Vec<AbsenceProof>,
}

#[derive(Debug)]
struct RequestedPath {
    path: ProjectSourcePath,
    delete_unconditionally: bool,
}

#[derive(Debug)]
struct ReadBlob {
    identity: StableFileIdentity,
    bytes: Option<BudgetedSourceBytes>,
    digest: Option<DigestV1>,
    hint: FileHint,
    snapshot: FileSnapshot,
    diagnostic: Option<ReadNote>,
}

#[derive(Debug, Clone, Copy)]
enum ReadNote {
    LimitExceeded {
        part: SourcePart,
        observed_at_least: u64,
        limit: u64,
    },
}

#[derive(Debug)]
enum ReadFailure {
    Anchored {
        part: SourcePart,
        source: AnchoredFsError,
    },
    Io {
        part: SourcePart,
        source: io::Error,
    },
    NotRegularFile {
        part: SourcePart,
    },
    Allocation {
        part: SourcePart,
        requested: u64,
    },
    Budget {
        part: SourcePart,
        source: BudgetError,
    },
    Changed {
        part: SourcePart,
        before: Option<FileHint>,
        after: Option<FileHint>,
    },
    Digest {
        part: SourcePart,
        source: DigestBuildError,
    },
}

#[derive(Debug, Clone, Copy)]
enum DiagnosticPath<'path> {
    Joined {
        root: &'path Path,
        relative: &'path Path,
    },
}

#[derive(Debug, Clone, Copy)]
enum DiagnosticMessage<'message> {
    Static(&'static str),
    Io(&'message io::Error),
}

#[derive(Debug, Clone, Copy)]
enum ReadDiagnosticSpec<'diagnostic> {
    PathRejected {
        path: DiagnosticPath<'diagnostic>,
        reason: PathRejection,
        part: SourcePart,
    },
    ReadFailed {
        rel_path: &'diagnostic str,
        part: SourcePart,
        kind: io::ErrorKind,
        message: DiagnosticMessage<'diagnostic>,
    },
    LimitExceeded {
        rel_path: &'diagnostic str,
        part: SourcePart,
        observed_at_least: u64,
        limit: u64,
    },
    AllocationFailed {
        rel_path: &'diagnostic str,
        part: SourcePart,
        requested: u64,
    },
    BudgetExceeded {
        rel_path: &'diagnostic str,
        part: SourcePart,
        source: &'diagnostic BudgetError,
    },
    ChangedDuringRead {
        rel_path: &'diagnostic str,
        part: SourcePart,
        before: Option<FileHint>,
        after: Option<FileHint>,
    },
    DigestFailed {
        rel_path: &'diagnostic str,
        part: SourcePart,
        source: &'diagnostic DigestBuildError,
    },
    MalformedGuid {
        rel_path: &'diagnostic str,
    },
    PayloadNotRetained {
        rel_path: &'diagnostic str,
        length: u64,
        retained_limit: u64,
    },
}

impl ReadDiagnosticSpec<'_> {
    const fn part(self) -> SourcePart {
        match self {
            Self::PathRejected { part, .. }
            | Self::ReadFailed { part, .. }
            | Self::LimitExceeded { part, .. }
            | Self::AllocationFailed { part, .. }
            | Self::BudgetExceeded { part, .. }
            | Self::ChangedDuringRead { part, .. }
            | Self::DigestFailed { part, .. } => part,
            Self::MalformedGuid { .. } | Self::PayloadNotRetained { .. } => SourcePart::Asset,
        }
    }
}

#[derive(Debug, Default)]
struct PendingReadDiagnostics {
    values: Vec<ScanDiagnostic>,
    dynamic_bytes: u64,
}

struct PreparedReadSourceBacking {
    rel_path: String,
    name: String,
    guid: Option<String>,
}

struct ReadBackingMaterializer<'budget> {
    budget: &'budget mut AssetLoadBudget,
    retained_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSnapshot {
    hint: FileHint,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    change_time_seconds: i64,
    #[cfg(unix)]
    change_time_nanoseconds: i64,
}

#[derive(Debug, Clone, Copy)]
struct SourceIdentityPart {
    digest: Option<DigestV1>,
    hint: FileHint,
}

impl From<&ReadBlob> for SourceIdentityPart {
    fn from(blob: &ReadBlob) -> Self {
        Self {
            digest: blob.digest,
            hint: blob.hint,
        }
    }
}

fn prepared_source_proof(
    relative: &str,
    asset: &ReadBlob,
    meta: Option<&ReadBlob>,
    meta_relative: PathBuf,
    budget: &AssetLoadBudget,
) -> std::result::Result<SourceProof, ScanError> {
    Ok(SourceProof {
        relative: prepared_string_clone(relative, budget, "scan source proof path")?,
        asset: FileProof {
            identity: asset.identity,
        },
        meta_relative,
        meta: meta.map(|meta| FileProof {
            identity: meta.identity,
        }),
    })
}

impl FileSnapshot {
    fn from_metadata(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;

        Self {
            hint: file_hint(metadata),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            change_time_seconds: metadata.ctime(),
            #[cfg(unix)]
            change_time_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

fn read_file_once(
    mut file: RegularFile,
    part: SourcePart,
    limit: u64,
    retained_limit: u64,
    metrics: &mut ScanMetrics,
    budget: &mut AssetLoadBudget,
) -> std::result::Result<ReadBlob, ReadFailure> {
    metrics.opened = metrics.opened.saturating_add(1);
    let identity = file.stable_identity();
    let before = file
        .file_mut()
        .metadata()
        .map_err(|source| ReadFailure::Io { part, source })?;
    if !before.is_file() {
        return Err(ReadFailure::NotRegularFile { part });
    }
    let before_snapshot = FileSnapshot::from_metadata(&before);
    let before_hint = before_snapshot.hint;
    if before_hint.size > limit {
        return Ok(ReadBlob {
            identity,
            bytes: None,
            digest: None,
            hint: before_hint,
            snapshot: before_snapshot,
            diagnostic: Some(ReadNote::LimitExceeded {
                part,
                observed_at_least: before_hint.size,
                limit,
            }),
        });
    }

    budget
        .consume_bytes(before_hint.size)
        .map_err(|source| ReadFailure::Budget { part, source })?;
    let mut bytes = if before_hint.size <= retained_limit {
        let initial_capacity =
            usize::try_from(before_hint.size).map_err(|_| ReadFailure::Allocation {
                part,
                requested: before_hint.size,
            })?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(initial_capacity)
            .map_err(|_| ReadFailure::Allocation {
                part,
                requested: before_hint.size,
            })?;
        let actual_capacity =
            u64::try_from(bytes.capacity()).map_err(|_| ReadFailure::Allocation {
                part,
                requested: u64::MAX,
            })?;
        let capacity_slack =
            actual_capacity
                .checked_sub(before_hint.size)
                .ok_or(ReadFailure::Allocation {
                    part,
                    requested: actual_capacity,
                })?;
        budget
            .consume_bytes(capacity_slack)
            .map_err(|source| ReadFailure::Budget { part, source })?;
        Some(bytes)
    } else {
        None
    };
    let mut digest = DigestV1Builder::new(before_hint.size);
    let mut observed = 0_u64;

    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    loop {
        let remaining_with_probe = before_hint
            .size
            .saturating_sub(observed)
            .saturating_add(1)
            .min(READ_BUFFER_BYTES as u64);
        let read_capacity = usize::try_from(remaining_with_probe).unwrap_or(READ_BUFFER_BYTES);
        let read = file
            .file_mut()
            .read(&mut buffer[..read_capacity])
            .map_err(|source| ReadFailure::Io { part, source })?;
        if read == 0 {
            break;
        }
        let read_u64 = u64::try_from(read).unwrap_or(u64::MAX);
        metrics.read_bytes = metrics.read_bytes.saturating_add(read_u64);
        observed = observed.saturating_add(read_u64);
        if observed > before_hint.size {
            let after = file
                .file_mut()
                .metadata()
                .map_err(|source| ReadFailure::Io { part, source })?;
            return Err(ReadFailure::Changed {
                part,
                before: Some(before_hint),
                after: Some(file_hint(&after)),
            });
        }
        digest
            .update(&buffer[..read])
            .map_err(|source| ReadFailure::Digest { part, source })?;
        if let Some(bytes) = bytes.as_mut() {
            bytes.extend_from_slice(&buffer[..read]);
        }
    }

    let after = file
        .file_mut()
        .metadata()
        .map_err(|source| ReadFailure::Io { part, source })?;
    let after_snapshot = FileSnapshot::from_metadata(&after);
    let identity_unchanged = file
        .same_identity(identity)
        .map_err(|source| ReadFailure::Anchored { part, source })?;
    if !identity_unchanged
        || before_snapshot != after_snapshot
        || after_snapshot.hint.size != observed
    {
        return Err(ReadFailure::Changed {
            part,
            before: Some(before_hint),
            after: Some(after_snapshot.hint),
        });
    }

    let digest = digest
        .finalize()
        .map_err(|source| ReadFailure::Digest { part, source })?;

    let bytes = bytes
        .map(|bytes| BudgetedSourceBytes::from_vec(bytes, budget))
        .transpose()
        .map_err(|source| ReadFailure::Budget { part, source })?;

    Ok(ReadBlob {
        identity,
        bytes,
        digest: Some(digest),
        hint: after_snapshot.hint,
        snapshot: after_snapshot,
        diagnostic: None,
    })
}

impl PendingReadDiagnostics {
    fn push(
        &mut self,
        spec: ReadDiagnosticSpec<'_>,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<(), ScanError> {
        let required = self
            .values
            .len()
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "read diagnostic entries",
            })?;
        let planned_dynamic = spec.planned_backing_bytes()?;
        let current_vector = checked_vec_bytes::<ScanDiagnostic>(self.values.capacity())?;
        let planned_capacity = self.values.capacity().max(required);
        let planned_vector = checked_vec_bytes::<ScanDiagnostic>(planned_capacity)?;
        let vector_growth =
            planned_vector
                .checked_sub(current_vector)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "read diagnostic list growth",
                })?;
        budget.check_entries(1)?;
        budget.check_bytes(checked_byte_add(vector_growth, planned_dynamic)?)?;
        budget.consume_entries(1)?;
        budget.consume_bytes(vector_growth)?;

        if required > self.values.capacity() {
            self.values
                .try_reserve_exact(1)
                .map_err(|source| ScanError::AllocationFailed {
                    allocation: "read diagnostic list",
                    requested: required,
                    source,
                })?;
        }

        let actual_vector = checked_vec_bytes::<ScanDiagnostic>(self.values.capacity())?;
        let vector_slack =
            actual_vector
                .checked_sub(planned_vector)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "read diagnostic list capacity",
                })?;
        budget.consume_bytes(vector_slack)?;
        let (diagnostic, materialized_retained) = {
            let mut materializer = ReadBackingMaterializer {
                budget,
                retained_bytes: 0,
            };
            let diagnostic = spec.materialize(&mut materializer)?;
            (diagnostic, materializer.retained_bytes)
        };

        let dynamic = diagnostic_backing_bytes(&diagnostic)?;
        let actual_dynamic = checked_byte_add(self.dynamic_bytes, dynamic)?;
        if materialized_retained != dynamic {
            return Err(ScanError::Budget(BudgetError::ArithmeticOverflow {
                resource: "read diagnostic retained bytes",
            }));
        }
        self.values.push(diagnostic);
        self.dynamic_bytes = actual_dynamic;
        Ok(())
    }

    fn into_values(self) -> Vec<ScanDiagnostic> {
        self.values
    }
}

impl ReadBackingMaterializer<'_> {
    fn charge_requested(&mut self, requested: u64) -> std::result::Result<(), ScanError> {
        self.budget.consume_bytes(requested)?;
        self.retained_bytes = checked_byte_add(self.retained_bytes, requested)?;
        Ok(())
    }

    fn charge_actual(&mut self, requested: u64, actual: u64) -> std::result::Result<(), ScanError> {
        let slack = actual
            .checked_sub(requested)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "read allocation capacity",
            })?;
        self.budget.consume_bytes(slack)?;
        self.retained_bytes = checked_byte_add(self.retained_bytes, slack)?;
        Ok(())
    }

    fn string_clone(
        &mut self,
        value: &str,
        allocation: &'static str,
    ) -> std::result::Result<String, ScanError> {
        let requested = checked_string_bytes(value.len())?;
        self.charge_requested(requested)?;
        let mut cloned = String::new();
        cloned
            .try_reserve_exact(value.len())
            .map_err(|source| ScanError::AllocationFailed {
                allocation,
                requested: value.len(),
                source,
            })?;
        self.charge_actual(requested, string_backing_bytes(&cloned)?)?;
        cloned.push_str(value);
        Ok(cloned)
    }

    fn joined_path(
        &mut self,
        root: &Path,
        relative: &Path,
    ) -> std::result::Result<PathBuf, ScanError> {
        let separator = usize::from(joined_path_needs_separator(root, relative));
        let requested = root
            .as_os_str()
            .len()
            .checked_add(separator)
            .and_then(|bytes| bytes.checked_add(relative.as_os_str().len()))
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "read diagnostic path",
            })?;
        let requested_bytes = checked_usize_bytes(requested)?;
        self.charge_requested(requested_bytes)?;
        let mut path = PathBuf::new();
        path.try_reserve_exact(requested)
            .map_err(|source| ScanError::AllocationFailed {
                allocation: "read diagnostic path",
                requested,
                source,
            })?;
        append_joined_path(&mut path, root, relative);
        self.charge_actual(requested_bytes, path_backing_bytes(&path)?)?;
        Ok(path)
    }

    fn display_string(
        &mut self,
        value: &dyn fmt::Display,
        allocation: &'static str,
    ) -> std::result::Result<String, ScanError> {
        let requested = display_len(value)?;
        let requested_bytes = checked_string_bytes(requested)?;
        self.charge_requested(requested_bytes)?;
        let mut output = String::new();
        output
            .try_reserve_exact(requested)
            .map_err(|source| ScanError::AllocationFailed {
                allocation,
                requested,
                source,
            })?;
        self.charge_actual(requested_bytes, string_backing_bytes(&output)?)?;
        let mut writer = FixedStringWriter {
            output,
            limit: requested,
        };
        fmt::write(&mut writer, format_args!("{value}")).map_err(|_| {
            ScanError::Budget(BudgetError::ArithmeticOverflow {
                resource: "read diagnostic formatting",
            })
        })?;
        Ok(writer.output)
    }

    fn lowercase_ascii_string(
        &mut self,
        value: &[u8],
        allocation: &'static str,
    ) -> std::result::Result<String, ScanError> {
        let requested = checked_string_bytes(value.len())?;
        self.charge_requested(requested)?;
        let mut output = String::new();
        output
            .try_reserve_exact(value.len())
            .map_err(|source| ScanError::AllocationFailed {
                allocation,
                requested: value.len(),
                source,
            })?;
        self.charge_actual(requested, string_backing_bytes(&output)?)?;
        for byte in value {
            output.push(char::from(byte.to_ascii_lowercase()));
        }
        Ok(output)
    }
}

fn prepare_read_source_backing(
    candidate: &ScanCandidate,
    guid_value: Option<&[u8]>,
    budget: &mut AssetLoadBudget,
) -> std::result::Result<PreparedReadSourceBacking, ScanError> {
    let source_layout = checked_usize_bytes(std::mem::size_of::<ReadSource>())?;
    let planned_strings = checked_byte_add(
        checked_byte_add(
            checked_string_bytes(candidate.relative_path().len())?,
            checked_string_bytes(candidate.name.len())?,
        )?,
        checked_string_bytes(guid_value.map_or(0, |value| value.len()))?,
    )?;
    budget.check_entries(1)?;
    budget.check_bytes(checked_byte_add(source_layout, planned_strings)?)?;
    budget.consume_entries(1)?;
    budget.consume_bytes(source_layout)?;
    let mut materializer = ReadBackingMaterializer {
        budget,
        retained_bytes: 0,
    };
    let rel_path =
        materializer.string_clone(candidate.relative_path(), "read source relative path")?;
    let name = materializer.string_clone(&candidate.name, "read source name")?;
    let guid = match guid_value {
        Some(value) => {
            Some(materializer.lowercase_ascii_string(value, "read source normalized GUID")?)
        }
        None => None,
    };
    Ok(PreparedReadSourceBacking {
        rel_path,
        name,
        guid,
    })
}

impl ReadDiagnosticSpec<'_> {
    fn planned_backing_bytes(self) -> std::result::Result<u64, ScanError> {
        match self {
            Self::PathRejected { path, .. } => path.planned_backing_bytes(),
            Self::ReadFailed {
                rel_path, message, ..
            } => checked_byte_add(
                checked_usize_bytes(rel_path.len())?,
                checked_usize_bytes(message.display_len()?)?,
            ),
            Self::DigestFailed {
                rel_path, source, ..
            } => checked_byte_add(
                checked_usize_bytes(rel_path.len())?,
                checked_usize_bytes(display_len(source)?)?,
            ),
            Self::LimitExceeded { rel_path, .. }
            | Self::AllocationFailed { rel_path, .. }
            | Self::BudgetExceeded { rel_path, .. }
            | Self::ChangedDuringRead { rel_path, .. }
            | Self::MalformedGuid { rel_path }
            | Self::PayloadNotRetained { rel_path, .. } => checked_usize_bytes(rel_path.len()),
        }
    }

    fn materialize(
        self,
        materializer: &mut ReadBackingMaterializer<'_>,
    ) -> std::result::Result<ScanDiagnostic, ScanError> {
        match self {
            Self::PathRejected { path, reason, .. } => Ok(ScanDiagnostic::PathRejected {
                path: path.materialize(materializer)?,
                reason,
            }),
            Self::ReadFailed {
                rel_path,
                part,
                kind,
                message,
            } => Ok(ScanDiagnostic::ReadFailed {
                rel_path: materializer.string_clone(rel_path, "read diagnostic relative path")?,
                part,
                kind,
                message: message.materialize(materializer)?,
            }),
            Self::LimitExceeded {
                rel_path,
                part,
                observed_at_least,
                limit,
            } => Ok(ScanDiagnostic::LimitExceeded {
                rel_path: materializer.string_clone(rel_path, "read diagnostic relative path")?,
                part,
                observed_at_least,
                limit,
            }),
            Self::AllocationFailed {
                rel_path,
                part,
                requested,
            } => Ok(ScanDiagnostic::AllocationFailed {
                rel_path: materializer.string_clone(rel_path, "read diagnostic relative path")?,
                part,
                requested,
            }),
            Self::BudgetExceeded {
                rel_path,
                part,
                source,
            } => Ok(ScanDiagnostic::BudgetExceeded {
                rel_path: materializer.string_clone(rel_path, "read diagnostic relative path")?,
                part,
                source: source.clone(),
            }),
            Self::ChangedDuringRead {
                rel_path,
                part,
                before,
                after,
            } => Ok(ScanDiagnostic::ChangedDuringRead {
                rel_path: materializer.string_clone(rel_path, "read diagnostic relative path")?,
                part,
                before,
                after,
            }),
            Self::DigestFailed {
                rel_path, source, ..
            } => Ok(ScanDiagnostic::DigestFailed {
                rel_path: materializer.string_clone(rel_path, "read diagnostic relative path")?,
                message: materializer.display_string(source, "read digest diagnostic message")?,
            }),
            Self::MalformedGuid { rel_path } => Ok(ScanDiagnostic::MalformedGuid {
                rel_path: materializer.string_clone(rel_path, "read diagnostic relative path")?,
            }),
            Self::PayloadNotRetained {
                rel_path,
                length,
                retained_limit,
            } => Ok(ScanDiagnostic::PayloadNotRetained {
                rel_path: materializer.string_clone(rel_path, "read diagnostic relative path")?,
                length,
                retained_limit,
            }),
        }
    }
}

impl DiagnosticPath<'_> {
    fn planned_backing_bytes(self) -> std::result::Result<u64, ScanError> {
        match self {
            Self::Joined { root, relative } => checked_usize_bytes(
                root.as_os_str()
                    .len()
                    .checked_add(1)
                    .and_then(|bytes| bytes.checked_add(relative.as_os_str().len()))
                    .ok_or(BudgetError::ArithmeticOverflow {
                        resource: "read diagnostic path",
                    })?,
            ),
        }
    }

    fn materialize(
        self,
        materializer: &mut ReadBackingMaterializer<'_>,
    ) -> std::result::Result<PathBuf, ScanError> {
        match self {
            Self::Joined { root, relative } => materializer.joined_path(root, relative),
        }
    }
}

impl DiagnosticMessage<'_> {
    fn display_len(self) -> std::result::Result<usize, ScanError> {
        match self {
            Self::Static(message) => Ok(message.len()),
            Self::Io(source) => display_len(source),
        }
    }

    fn materialize(
        self,
        materializer: &mut ReadBackingMaterializer<'_>,
    ) -> std::result::Result<String, ScanError> {
        match self {
            Self::Static(message) => materializer.string_clone(message, "read diagnostic message"),
            Self::Io(source) => materializer.display_string(source, "read I/O diagnostic message"),
        }
    }
}

fn read_note_spec<'diagnostic>(
    note: ReadNote,
    rel_path: &'diagnostic str,
) -> ReadDiagnosticSpec<'diagnostic> {
    match note {
        ReadNote::LimitExceeded {
            part,
            observed_at_least,
            limit,
        } => ReadDiagnosticSpec::LimitExceeded {
            rel_path,
            part,
            observed_at_least,
            limit,
        },
    }
}

fn rejected_read_failure<'diagnostic>(
    failure: &'diagnostic ReadFailure,
    rel_path: &'diagnostic str,
    diagnostic_path: DiagnosticPath<'diagnostic>,
    metrics: ScanMetrics,
    budget: &mut AssetLoadBudget,
) -> ReadSourceOutcome {
    let spec = match failure {
        ReadFailure::Anchored {
            part,
            source: AnchoredFsError::LinkOrReparse,
        } => ReadDiagnosticSpec::PathRejected {
            path: diagnostic_path,
            reason: PathRejection::Symlink,
            part: *part,
        },
        ReadFailure::Anchored {
            part,
            source: AnchoredFsError::IdentityChanged,
        } => ReadDiagnosticSpec::ChangedDuringRead {
            rel_path,
            part: *part,
            before: None,
            after: None,
        },
        ReadFailure::Anchored {
            part,
            source: AnchoredFsError::NotDirectory | AnchoredFsError::NotRegular,
        } => ReadDiagnosticSpec::ReadFailed {
            rel_path,
            part: *part,
            kind: io::ErrorKind::InvalidInput,
            message: DiagnosticMessage::Static("anchored source is not a regular file"),
        },
        ReadFailure::Anchored {
            part,
            source: AnchoredFsError::UnsupportedPlatform,
        } => ReadDiagnosticSpec::ReadFailed {
            rel_path,
            part: *part,
            kind: io::ErrorKind::Unsupported,
            message: DiagnosticMessage::Static(
                "anchored project-source reads are unsupported on this platform",
            ),
        },
        ReadFailure::Anchored {
            part,
            source: AnchoredFsError::UnsupportedCaseSensitiveDirectory,
        } => ReadDiagnosticSpec::PathRejected {
            path: diagnostic_path,
            reason: PathRejection::UnsupportedCaseSensitiveDirectory,
            part: *part,
        },
        ReadFailure::Anchored {
            part,
            source: AnchoredFsError::Io(source),
        } if source.kind() == io::ErrorKind::InvalidInput => ReadDiagnosticSpec::PathRejected {
            path: diagnostic_path,
            reason: PathRejection::InvalidPath,
            part: *part,
        },
        ReadFailure::Anchored {
            part,
            source: AnchoredFsError::Io(source),
        } => ReadDiagnosticSpec::ReadFailed {
            rel_path,
            part: *part,
            kind: source.kind(),
            message: DiagnosticMessage::Io(source),
        },
        ReadFailure::Io { part, source } => ReadDiagnosticSpec::ReadFailed {
            rel_path,
            part: *part,
            kind: source.kind(),
            message: DiagnosticMessage::Io(source),
        },
        ReadFailure::NotRegularFile { part } => ReadDiagnosticSpec::ReadFailed {
            rel_path,
            part: *part,
            kind: io::ErrorKind::InvalidInput,
            message: DiagnosticMessage::Static("opened project source is not a regular file"),
        },
        ReadFailure::Allocation { part, requested } => ReadDiagnosticSpec::AllocationFailed {
            rel_path,
            part: *part,
            requested: *requested,
        },
        ReadFailure::Budget { part, source } => ReadDiagnosticSpec::BudgetExceeded {
            rel_path,
            part: *part,
            source,
        },
        ReadFailure::Changed {
            part,
            before,
            after,
        } => ReadDiagnosticSpec::ChangedDuringRead {
            rel_path,
            part: *part,
            before: *before,
            after: *after,
        },
        ReadFailure::Digest { part, source } => ReadDiagnosticSpec::DigestFailed {
            rel_path,
            part: *part,
            source,
        },
    };
    rejected_with_spec(spec, metrics, budget)
}

fn rejected_with_spec(
    spec: ReadDiagnosticSpec<'_>,
    metrics: ScanMetrics,
    budget: &mut AssetLoadBudget,
) -> ReadSourceOutcome {
    let part = spec.part();
    match try_rejected_with_spec(spec, &metrics, budget) {
        Ok(outcome) => outcome,
        Err(error) => rejected_scan_error(error, part, metrics, budget),
    }
}

fn try_rejected_with_spec(
    spec: ReadDiagnosticSpec<'_>,
    metrics: &ScanMetrics,
    budget: &mut AssetLoadBudget,
) -> std::result::Result<ReadSourceOutcome, ScanError> {
    let mut pending = PendingReadDiagnostics::default();
    pending.push(spec, budget)?;
    Ok(ReadSourceOutcome {
        accepted: None,
        diagnostics: pending.into_values(),
        metrics: metrics.clone(),
    })
}

fn rejected_scan_error(
    error: ScanError,
    part: SourcePart,
    metrics: ScanMetrics,
    budget: &mut AssetLoadBudget,
) -> ReadSourceOutcome {
    let fallback = match &error {
        ScanError::Budget(source) => ReadDiagnosticSpec::BudgetExceeded {
            rel_path: "",
            part,
            source,
        },
        ScanError::AllocationFailed { requested, .. } => ReadDiagnosticSpec::AllocationFailed {
            rel_path: "",
            part,
            requested: u64::try_from(*requested).unwrap_or(u64::MAX),
        },
        ScanError::Policy(_)
        | ScanError::PolicyRead { .. }
        | ScanError::PolicyChangedDuringScan
        | ScanError::TraversalRead { .. }
        | ScanError::TraversalChangedDuringScan
        | ScanError::SourceChangedDuringScan
        | ScanError::TraversalLimitExceeded { .. }
        | ScanError::ChangedPathProjectMismatch { .. }
        | ScanError::ProjectPath(_) => ReadDiagnosticSpec::ReadFailed {
            rel_path: "",
            part,
            kind: io::ErrorKind::InvalidData,
            message: DiagnosticMessage::Static("scanner policy or traversal failed"),
        },
    };
    try_rejected_with_spec(fallback, &metrics, budget)
        .unwrap_or_else(|_| ReadSourceOutcome::rejected_without_diagnostic(metrics))
}

fn display_len(value: &dyn fmt::Display) -> std::result::Result<usize, ScanError> {
    let mut counter = DisplayLength::default();
    fmt::write(&mut counter, format_args!("{value}")).map_err(|_| {
        ScanError::Budget(BudgetError::ArithmeticOverflow {
            resource: "read diagnostic formatting",
        })
    })?;
    counter.len.ok_or({
        ScanError::Budget(BudgetError::ArithmeticOverflow {
            resource: "read diagnostic formatting",
        })
    })
}

#[derive(Debug)]
struct DisplayLength {
    len: Option<usize>,
}

impl Default for DisplayLength {
    fn default() -> Self {
        Self { len: Some(0) }
    }
}

impl fmt::Write for DisplayLength {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let Some(current) = self.len else {
            return Err(fmt::Error);
        };
        self.len = current.checked_add(value.len());
        if self.len.is_some() {
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }
}

#[derive(Debug)]
struct FixedStringWriter {
    output: String,
    limit: usize,
}

impl fmt::Write for FixedStringWriter {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let required = self
            .output
            .len()
            .checked_add(value.len())
            .ok_or(fmt::Error)?;
        if required > self.limit {
            return Err(fmt::Error);
        }
        self.output.push_str(value);
        Ok(())
    }
}

fn source_identity(
    asset: SourceIdentityPart,
    meta: Option<SourceIdentityPart>,
) -> std::result::Result<DigestV1, DigestBuildError> {
    let asset_size = asset.hint.size.to_le_bytes();
    let asset_mtime = asset.hint.mtime_ms.unwrap_or(u64::MAX).to_le_bytes();
    let meta_size = meta.as_ref().map_or(0, |meta| meta.hint.size).to_le_bytes();
    let meta_mtime = meta
        .as_ref()
        .and_then(|meta| meta.hint.mtime_ms)
        .unwrap_or(u64::MAX)
        .to_le_bytes();
    let mut components = [&[][..]; 9];
    let mut component_count = 0;
    components[component_count] = SOURCE_IDENTITY_DOMAIN;
    component_count += 1;
    match asset.digest.as_ref() {
        Some(digest) => {
            components[component_count] = ASSET_COMPLETE;
            component_count += 1;
            components[component_count] = digest.as_bytes();
            component_count += 1;
        }
        None => {
            components[component_count] = ASSET_UNAVAILABLE;
            component_count += 1;
            components[component_count] = &asset_size;
            component_count += 1;
            components[component_count] = &asset_mtime;
            component_count += 1;
        }
    }
    match meta.as_ref() {
        None => {
            components[component_count] = META_ABSENT;
            component_count += 1;
        }
        Some(meta) => match meta.digest.as_ref() {
            Some(digest) => {
                components[component_count] = META_PRESENT;
                component_count += 1;
                components[component_count] = digest.as_bytes();
                component_count += 1;
            }
            None => {
                components[component_count] = META_UNAVAILABLE;
                component_count += 1;
                components[component_count] = &meta_size;
                component_count += 1;
                components[component_count] = &meta_mtime;
                component_count += 1;
            }
        },
    }
    let components = &components[..component_count];
    let declared_length = components.iter().try_fold(0_u64, |total, component| {
        total
            .checked_add(DigestV1Builder::framed_len(component)?)
            .ok_or(DigestBuildError::LengthOverflow)
    })?;
    let mut builder = DigestV1Builder::new(declared_length);
    for component in components {
        builder.update_framed(component)?;
    }
    builder.finalize()
}

fn guid_value_from_meta(meta: &[u8]) -> (Option<&[u8]>, bool) {
    let mut found = None;
    for line in meta.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.first().is_some_and(u8::is_ascii_whitespace) {
            continue;
        }
        let Some(value) = line.strip_prefix(b"guid:") else {
            continue;
        };
        let value = trim_ascii(value);
        if value.len() != 32 || !value.iter().all(u8::is_ascii_hexdigit) || found.is_some() {
            return (None, true);
        }
        found = Some(value);
    }
    match found {
        Some(guid) => (Some(guid), false),
        None => (None, true),
    }
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn file_hint(metadata: &Metadata) -> FileHint {
    FileHint {
        size: metadata.len(),
        mtime_ms: metadata.modified().ok().and_then(system_time_millis),
    }
}

fn system_time_millis(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn append_missing_known_paths(
    known: &[ProjectSourcePath],
    present: &[ScanCandidate],
    deleted: &mut Vec<ProjectSourcePath>,
    budget: &mut AssetLoadBudget,
) -> std::result::Result<(), ScanError> {
    for path in known {
        if contains_candidate(present, path) {
            continue;
        }
        let path = prepared_project_source_path_clone(path, budget, "known deletion path")?;
        push_retained(
            deleted,
            path,
            project_source_path_backing_bytes,
            budget,
            "known deletion list",
        )?;
    }
    Ok(())
}

fn sort_and_dedup_candidates(candidates: &mut Vec<ScanCandidate>) {
    candidates.sort_unstable_by(compare_candidates);
    candidates
        .dedup_by(|left, right| portable_paths_equal(left.relative_path(), right.relative_path()));
}

fn sort_directory_entries(entries: &mut [DirectoryEntryHint]) {
    entries.sort_unstable_by(|left, right| left.name().cmp(right.name()));
}

fn preflight_directory_entries(
    directory: &ReadDirectory,
    parent: &str,
    child_depth: u32,
    ledger: &mut ScanLedger,
    budget: &AssetLoadBudget,
) -> std::result::Result<DirectoryEntryPreflight, ScanError> {
    let mut preflight = DirectoryEntryPreflight {
        count: 0,
        path_bytes: 0,
        retained_name_bytes: 0,
    };
    let names = directory
        .entry_names()
        .map_err(|source| ScanError::TraversalRead { source })?;
    for name in names {
        let name = name.map_err(|source| ScanError::TraversalRead { source })?;
        preflight.count =
            preflight
                .count
                .checked_add(1)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "scan directory entry count",
                })?;
        ledger.check_additional_entries(preflight.count)?;
        budget.check_entries(preflight.count)?;
        preflight.path_bytes = preflight
            .path_bytes
            .checked_add(directory_entry_path_bytes(parent, &name)?)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "scan directory entry path bytes",
            })?;
        ledger.check_additional_path_bytes(preflight.path_bytes)?;
        preflight.retained_name_bytes = preflight
            .retained_name_bytes
            .checked_add(checked_usize_bytes(name.capacity())?)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "scan directory entry name bytes",
            })?;
    }
    ledger.observe_entries(preflight.count, preflight.path_bytes, child_depth)?;
    Ok(preflight)
}

fn collect_preflighted_directory_entries(
    directory: &ReadDirectory,
    parent: &str,
    preflight: &DirectoryEntryPreflight,
    expected_identity: StableDirectoryIdentity,
    budget: &mut AssetLoadBudget,
) -> std::result::Result<Vec<DirectoryEntryHint>, ScanError> {
    let count = usize::try_from(preflight.count).map_err(|_| {
        ScanError::Budget(BudgetError::ArithmeticOverflow {
            resource: "scan directory entry capacity",
        })
    })?;
    let planned_vector_bytes = checked_vec_bytes::<DirectoryEntryHint>(count)?;
    let planned_bytes = checked_byte_add(preflight.retained_name_bytes, planned_vector_bytes)?;
    budget.check_entries(preflight.count)?;
    budget.check_bytes(planned_bytes)?;

    let mut retained = Vec::new();
    retained
        .try_reserve_exact(count)
        .map_err(|source| ScanError::AllocationFailed {
            allocation: "directory entry hint list",
            requested: count,
            source,
        })?;
    let actual_vector_bytes = checked_vec_bytes::<DirectoryEntryHint>(retained.capacity())?;
    let charged_bytes = checked_byte_add(preflight.retained_name_bytes, actual_vector_bytes)?;
    budget.check_bytes(charged_bytes)?;
    budget.consume_entries(preflight.count)?;
    budget.consume_bytes(charged_bytes)?;

    let mut observed_count = 0_u64;
    let mut observed_path_bytes = 0_u64;
    let mut observed_name_bytes = 0_u64;
    let entries = directory
        .entries()
        .map_err(|source| ScanError::TraversalRead { source })?;
    for entry in entries {
        let entry = entry.map_err(|source| ScanError::TraversalRead { source })?;
        observed_count = observed_count
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "scan directory entry count",
            })?;
        if observed_count > preflight.count {
            return Err(ScanError::TraversalChangedDuringScan);
        }
        observed_path_bytes = observed_path_bytes
            .checked_add(directory_entry_path_bytes(parent, entry.name())?)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "scan directory entry path bytes",
            })?;
        observed_name_bytes = observed_name_bytes
            .checked_add(directory_entry_backing_bytes(&entry)?)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "scan directory entry name bytes",
            })?;
        if observed_name_bytes > preflight.retained_name_bytes {
            return Err(ScanError::TraversalChangedDuringScan);
        }
        retained.push(entry);
    }
    if observed_count != preflight.count || observed_path_bytes != preflight.path_bytes {
        return Err(ScanError::TraversalChangedDuringScan);
    }
    directory
        .ensure_identity(expected_identity)
        .map_err(directory_revalidation_error)?;
    Ok(retained)
}

fn directory_entry_path_bytes(parent: &str, name: &OsStr) -> Result<u64, ScanError> {
    let name_bytes = name.to_str().map_or_else(|| name.len(), str::len);
    let path_bytes = parent
        .len()
        .checked_add(usize::from(!parent.is_empty()))
        .and_then(|length| length.checked_add(name_bytes))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "scan directory entry path bytes",
        })?;
    u64::try_from(path_bytes).map_err(|_| {
        ScanError::Budget(BudgetError::ArithmeticOverflow {
            resource: "scan directory entry path bytes",
        })
    })
}

fn compare_candidates(left: &ScanCandidate, right: &ScanCandidate) -> Ordering {
    compare_portable_paths(left.relative_path(), right.relative_path())
        .then_with(|| left.relative_path().cmp(right.relative_path()))
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.kind.canonical_name().cmp(right.kind.canonical_name()))
}

fn contains_candidate(candidates: &[ScanCandidate], path: &ProjectSourcePath) -> bool {
    candidates
        .binary_search_by(|candidate| {
            compare_portable_paths(candidate.relative_path(), path.relative_path())
        })
        .ok()
        .and_then(|index| candidates.get(index))
        .is_some_and(|candidate| candidate.coordinate() == path.coordinate())
}

fn push_diagnostic(
    diagnostics: &mut Vec<ScanDiagnostic>,
    diagnostic: ScanDiagnostic,
    budget: &mut AssetLoadBudget,
) -> std::result::Result<(), ScanError> {
    push_retained(
        diagnostics,
        diagnostic,
        diagnostic_backing_bytes,
        budget,
        "scan diagnostic list",
    )
}

fn push_scan_diagnostic(
    diagnostics: &mut Vec<ScanDiagnostic>,
    diagnostic: ScanDiagnostic,
    ledger: &mut ScanLedger,
    budget: &mut AssetLoadBudget,
) -> std::result::Result<(), ScanError> {
    ledger.observe_diagnostic()?;
    push_diagnostic(diagnostics, diagnostic, budget)
}

fn push_anchored_diagnostic(
    diagnostics: &mut Vec<ScanDiagnostic>,
    project_root: &Path,
    relative: &Path,
    source: AnchoredFsError,
    ledger: &mut ScanLedger,
    budget: &mut AssetLoadBudget,
) -> std::result::Result<(), ScanError> {
    let diagnostic = match source {
        AnchoredFsError::LinkOrReparse => ScanDiagnostic::PathRejected {
            path: prepared_joined_path(
                project_root,
                relative,
                budget,
                "anchored link diagnostic path",
            )?,
            reason: PathRejection::Symlink,
        },
        AnchoredFsError::NotDirectory | AnchoredFsError::NotRegular => {
            ScanDiagnostic::PathRejected {
                path: prepared_joined_path(
                    project_root,
                    relative,
                    budget,
                    "anchored type diagnostic path",
                )?,
                reason: PathRejection::UnsupportedFileType,
            }
        }
        AnchoredFsError::Io(source) => {
            let rel_path = prepared_portable_relative_path(
                relative,
                budget,
                "anchored I/O diagnostic relative path",
            )?
            .unwrap_or_default();
            let mut materializer = ReadBackingMaterializer {
                budget,
                retained_bytes: 0,
            };
            let message =
                materializer.display_string(&source, "anchored I/O diagnostic message")?;
            ScanDiagnostic::ReadFailed {
                rel_path,
                part: SourcePart::Asset,
                kind: source.kind(),
                message,
            }
        }
        AnchoredFsError::UnsupportedPlatform => ScanDiagnostic::WalkFailed {
            message: prepared_string_clone(
                "identity-bound scanning is unsupported on this platform",
                budget,
                "unsupported-platform diagnostic",
            )?,
        },
        source @ AnchoredFsError::UnsupportedCaseSensitiveDirectory => {
            return Err(ScanError::TraversalRead { source });
        }
        AnchoredFsError::IdentityChanged => ScanDiagnostic::WalkFailed {
            message: prepared_string_clone(
                "anchored filesystem identity changed during traversal",
                budget,
                "identity-change diagnostic",
            )?,
        },
    };
    push_scan_diagnostic(diagnostics, diagnostic, ledger, budget)
}

fn directory_revalidation_error(source: AnchoredFsError) -> ScanError {
    if is_revalidation_change(&source) {
        ScanError::TraversalChangedDuringScan
    } else {
        ScanError::TraversalRead { source }
    }
}

fn source_revalidation_error(source: AnchoredFsError) -> ScanError {
    if is_revalidation_change(&source) {
        ScanError::SourceChangedDuringScan
    } else {
        ScanError::TraversalRead { source }
    }
}

fn is_revalidation_change(source: &AnchoredFsError) -> bool {
    matches!(
        source,
        AnchoredFsError::IdentityChanged
            | AnchoredFsError::LinkOrReparse
            | AnchoredFsError::NotDirectory
            | AnchoredFsError::NotRegular
    ) || matches!(
        source,
        AnchoredFsError::Io(error) if error.kind() == io::ErrorKind::NotFound
    )
}

fn push_retained<T>(
    values: &mut Vec<T>,
    value: T,
    retained_backing_bytes: fn(&T) -> std::result::Result<u64, ScanError>,
    budget: &mut AssetLoadBudget,
    allocation: &'static str,
) -> std::result::Result<(), ScanError> {
    let retained_bytes = retained_backing_bytes(&value)?;
    budget.check_entries(1)?;
    budget.check_bytes(retained_bytes)?;

    let previous_capacity = values.capacity();
    if values.len() == previous_capacity {
        let required = values
            .len()
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "scan collection capacity",
            })?;
        let planned_growth = checked_vec_bytes::<T>(1)?;
        budget.check_bytes(checked_byte_add(retained_bytes, planned_growth)?)?;
        values
            .try_reserve_exact(1)
            .map_err(|source| ScanError::AllocationFailed {
                allocation,
                requested: required,
                source,
            })?;
    }

    let added_capacity = values.capacity().checked_sub(previous_capacity).ok_or(
        BudgetError::ArithmeticOverflow {
            resource: "scan collection capacity",
        },
    )?;
    let vector_bytes = checked_vec_bytes::<T>(added_capacity)?;
    let charged_bytes = checked_byte_add(retained_bytes, vector_bytes)?;
    budget.check_bytes(charged_bytes)?;
    budget.consume_entries(1)?;
    budget.consume_bytes(charged_bytes)?;
    values.push(value);
    Ok(())
}

fn prepared_string_clone(
    value: &str,
    budget: &AssetLoadBudget,
    allocation: &'static str,
) -> std::result::Result<String, ScanError> {
    let planned_bytes = checked_string_bytes(value.len())?;
    budget.check_bytes(planned_bytes)?;
    let mut cloned = String::new();
    cloned
        .try_reserve_exact(value.len())
        .map_err(|source| ScanError::AllocationFailed {
            allocation,
            requested: value.len(),
            source,
        })?;
    cloned.push_str(value);
    budget.check_bytes(string_backing_bytes(&cloned)?)?;
    Ok(cloned)
}

fn prepared_project_source_path_clone(
    path: &ProjectSourcePath,
    budget: &AssetLoadBudget,
    allocation: &'static str,
) -> std::result::Result<ProjectSourcePath, ScanError> {
    let relative_path = prepared_string_clone(path.relative_path(), budget, allocation)?;
    Ok(ProjectSourcePath::from_validated_parts(
        path.identity(),
        relative_path,
    ))
}

fn prepared_path_clone(
    value: &Path,
    budget: &AssetLoadBudget,
    allocation: &'static str,
) -> std::result::Result<PathBuf, ScanError> {
    let requested = value.as_os_str().len();
    budget.check_bytes(checked_usize_bytes(requested)?)?;
    let mut cloned = PathBuf::new();
    cloned
        .try_reserve_exact(requested)
        .map_err(|source| ScanError::AllocationFailed {
            allocation,
            requested,
            source,
        })?;
    cloned.push(value);
    budget.check_bytes(path_backing_bytes(&cloned)?)?;
    Ok(cloned)
}

fn prepared_joined_path(
    root: &Path,
    relative: &Path,
    budget: &mut AssetLoadBudget,
    allocation: &'static str,
) -> std::result::Result<PathBuf, ScanError> {
    let separator = usize::from(joined_path_needs_separator(root, relative));
    let requested = root
        .as_os_str()
        .len()
        .checked_add(separator)
        .and_then(|bytes| bytes.checked_add(relative.as_os_str().len()))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "scan joined path bytes",
        })?;
    budget.check_bytes(checked_usize_bytes(requested)?)?;
    let mut path = PathBuf::new();
    path.try_reserve_exact(requested)
        .map_err(|source| ScanError::AllocationFailed {
            allocation,
            requested,
            source,
        })?;
    let reserved = path_backing_bytes(&path)?;
    budget.check_bytes(reserved)?;
    append_joined_path(&mut path, root, relative);
    let actual = path_backing_bytes(&path)?;
    if actual > reserved {
        return Err(ScanError::Budget(BudgetError::ArithmeticOverflow {
            resource: "scan joined path allocation",
        }));
    }
    budget.consume_bytes(actual)?;
    Ok(path)
}

fn joined_path_needs_separator(root: &Path, relative: &Path) -> bool {
    !root.as_os_str().is_empty()
        && !relative.as_os_str().is_empty()
        && !root
            .as_os_str()
            .as_encoded_bytes()
            .last()
            .is_some_and(|byte| matches!(*byte, b'/' | b'\\'))
}

#[cfg(not(windows))]
fn append_joined_path(path: &mut PathBuf, root: &Path, relative: &Path) {
    path.as_mut_os_string().push(root.as_os_str());
    if joined_path_needs_separator(root, relative) {
        path.as_mut_os_string().push(std::path::MAIN_SEPARATOR_STR);
    }
    path.as_mut_os_string().push(relative.as_os_str());
}

#[cfg(windows)]
fn append_joined_path(path: &mut PathBuf, root: &Path, relative: &Path) {
    path.as_mut_os_string().push(root.as_os_str());
    for component in relative.components() {
        if !path.as_os_str().is_empty()
            && !path
                .as_os_str()
                .as_encoded_bytes()
                .last()
                .is_some_and(|byte| matches!(*byte, b'/' | b'\\'))
        {
            path.as_mut_os_string().push(std::path::MAIN_SEPARATOR_STR);
        }
        path.as_mut_os_string().push(component.as_os_str());
    }
}

fn prepared_relative_child_path(
    parent: &Path,
    name: &OsStr,
    budget: &AssetLoadBudget,
    allocation: &'static str,
) -> std::result::Result<PathBuf, ScanError> {
    let separator = usize::from(!parent.as_os_str().is_empty());
    let requested = parent
        .as_os_str()
        .len()
        .checked_add(separator)
        .and_then(|bytes| bytes.checked_add(name.len()))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "scan relative child path bytes",
        })?;
    budget.check_bytes(checked_usize_bytes(requested)?)?;
    let mut path = PathBuf::new();
    path.try_reserve_exact(requested)
        .map_err(|source| ScanError::AllocationFailed {
            allocation,
            requested,
            source,
        })?;
    budget.check_bytes(path_backing_bytes(&path)?)?;
    if !parent.as_os_str().is_empty() {
        path.push(parent);
    }
    path.push(name);
    Ok(path)
}

fn prepared_portable_child_path(
    parent: &str,
    name: &str,
    budget: &AssetLoadBudget,
    allocation: &'static str,
) -> std::result::Result<String, ScanError> {
    let separator = usize::from(!parent.is_empty());
    let requested = parent
        .len()
        .checked_add(separator)
        .and_then(|bytes| bytes.checked_add(name.len()))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "scan portable child path bytes",
        })?;
    budget.check_bytes(checked_string_bytes(requested)?)?;
    let mut path = String::new();
    path.try_reserve_exact(requested)
        .map_err(|source| ScanError::AllocationFailed {
            allocation,
            requested,
            source,
        })?;
    budget.check_bytes(string_backing_bytes(&path)?)?;
    path.push_str(parent);
    if !parent.is_empty() {
        path.push('/');
    }
    path.push_str(name);
    Ok(path)
}

fn prepared_portable_relative_path(
    relative: &Path,
    budget: &AssetLoadBudget,
    allocation: &'static str,
) -> std::result::Result<std::result::Result<String, PathRejection>, ScanError> {
    let requested = relative.as_os_str().len();
    budget.check_bytes(checked_string_bytes(requested)?)?;
    let mut output = String::new();
    output
        .try_reserve_exact(requested)
        .map_err(|source| ScanError::AllocationFailed {
            allocation,
            requested,
            source,
        })?;
    budget.check_bytes(string_backing_bytes(&output)?)?;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            if matches!(component, Component::CurDir) {
                continue;
            }
            return Ok(Err(PathRejection::InvalidPath));
        };
        let Some(component) = component.to_str() else {
            return Ok(Err(PathRejection::NonUtf8RelativePath));
        };
        if !is_portable_component(component) {
            return Ok(Err(PathRejection::InvalidPath));
        }
        if !output.is_empty() {
            output.push('/');
        }
        output.push_str(component);
    }
    Ok(Ok(output))
}

fn candidate_backing_bytes(candidate: &ScanCandidate) -> std::result::Result<u64, ScanError> {
    checked_byte_add(
        checked_string_bytes(candidate.relative_path_capacity())?,
        string_backing_bytes(&candidate.name)?,
    )
}

fn pending_directory_backing_bytes(
    directory: &PendingDirectory,
) -> std::result::Result<u64, ScanError> {
    checked_byte_add(
        path_backing_bytes(&directory.relative)?,
        string_backing_bytes(&directory.normalized)?,
    )
}

fn directory_proof_backing_bytes(proof: &DirectoryProof) -> std::result::Result<u64, ScanError> {
    path_backing_bytes(&proof.relative)
}

fn source_proof_backing_bytes(proof: &SourceProof) -> std::result::Result<u64, ScanError> {
    // `meta_relative` is charged when the read path is prepared because it is needed even when
    // the read fails. Retaining the successful proof must not charge that same allocation twice.
    string_backing_bytes(&proof.relative)
}

fn absence_proof_backing_bytes(proof: &AbsenceProof) -> std::result::Result<u64, ScanError> {
    string_backing_bytes(&proof.relative)
}

fn directory_entry_backing_bytes(
    entry: &DirectoryEntryHint,
) -> std::result::Result<u64, ScanError> {
    checked_usize_bytes(entry.name_capacity())
}

fn requested_path_backing_bytes(requested: &RequestedPath) -> std::result::Result<u64, ScanError> {
    project_source_path_backing_bytes(&requested.path)
}

fn project_source_path_backing_bytes(
    path: &ProjectSourcePath,
) -> std::result::Result<u64, ScanError> {
    checked_string_bytes(path.relative_path_capacity())
}

fn diagnostic_backing_bytes(diagnostic: &ScanDiagnostic) -> std::result::Result<u64, ScanError> {
    match diagnostic {
        ScanDiagnostic::WalkFailed { message } => string_backing_bytes(message),
        ScanDiagnostic::PathRejected { path, .. } => path_backing_bytes(path),
        ScanDiagnostic::ReadFailed {
            rel_path, message, ..
        }
        | ScanDiagnostic::DigestFailed { rel_path, message } => checked_byte_add(
            string_backing_bytes(rel_path)?,
            string_backing_bytes(message)?,
        ),
        ScanDiagnostic::LimitExceeded { rel_path, .. }
        | ScanDiagnostic::AllocationFailed { rel_path, .. }
        | ScanDiagnostic::BudgetExceeded { rel_path, .. }
        | ScanDiagnostic::ChangedDuringRead { rel_path, .. }
        | ScanDiagnostic::MalformedGuid { rel_path }
        | ScanDiagnostic::PayloadNotRetained { rel_path, .. } => string_backing_bytes(rel_path),
    }
}

fn string_backing_bytes(value: &String) -> std::result::Result<u64, ScanError> {
    checked_string_bytes(value.capacity())
}

fn path_backing_bytes(value: &PathBuf) -> std::result::Result<u64, ScanError> {
    checked_usize_bytes(value.capacity())
}

fn checked_string_bytes(capacity: usize) -> std::result::Result<u64, ScanError> {
    string_allocation_bytes(capacity).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "scan retained bytes",
        }
        .into()
    })
}

fn checked_vec_bytes<T>(capacity: usize) -> std::result::Result<u64, ScanError> {
    vec_allocation_bytes::<T>(capacity).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "scan retained bytes",
        }
        .into()
    })
}

fn checked_usize_bytes(value: usize) -> std::result::Result<u64, ScanError> {
    u64::try_from(value).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "scan retained bytes",
        }
        .into()
    })
}

fn checked_byte_add(left: u64, right: u64) -> std::result::Result<u64, ScanError> {
    left.checked_add(right).ok_or_else(|| {
        BudgetError::ArithmeticOverflow {
            resource: "scan retained bytes",
        }
        .into()
    })
}

fn compare_scan_diagnostics(left: &ScanDiagnostic, right: &ScanDiagnostic) -> Ordering {
    diagnostic_rank(left)
        .cmp(&diagnostic_rank(right))
        .then_with(|| match (left, right) {
            (
                ScanDiagnostic::WalkFailed {
                    message: left_message,
                },
                ScanDiagnostic::WalkFailed {
                    message: right_message,
                },
            ) => left_message.cmp(right_message),
            (
                ScanDiagnostic::PathRejected {
                    path: left_path,
                    reason: left_reason,
                },
                ScanDiagnostic::PathRejected {
                    path: right_path,
                    reason: right_reason,
                },
            ) => left_path
                .cmp(right_path)
                .then_with(|| left_reason.cmp(right_reason)),
            (
                ScanDiagnostic::ReadFailed {
                    rel_path: left_path,
                    part: left_part,
                    kind: left_kind,
                    message: left_message,
                },
                ScanDiagnostic::ReadFailed {
                    rel_path: right_path,
                    part: right_part,
                    kind: right_kind,
                    message: right_message,
                },
            ) => left_path
                .cmp(right_path)
                .then_with(|| left_part.cmp(right_part))
                .then_with(|| left_kind.cmp(right_kind))
                .then_with(|| left_message.cmp(right_message)),
            (
                ScanDiagnostic::LimitExceeded {
                    rel_path: left_path,
                    part: left_part,
                    observed_at_least: left_observed,
                    limit: left_limit,
                },
                ScanDiagnostic::LimitExceeded {
                    rel_path: right_path,
                    part: right_part,
                    observed_at_least: right_observed,
                    limit: right_limit,
                },
            ) => left_path
                .cmp(right_path)
                .then_with(|| left_part.cmp(right_part))
                .then_with(|| left_observed.cmp(right_observed))
                .then_with(|| left_limit.cmp(right_limit)),
            (
                ScanDiagnostic::AllocationFailed {
                    rel_path: left_path,
                    part: left_part,
                    requested: left_requested,
                },
                ScanDiagnostic::AllocationFailed {
                    rel_path: right_path,
                    part: right_part,
                    requested: right_requested,
                },
            ) => left_path
                .cmp(right_path)
                .then_with(|| left_part.cmp(right_part))
                .then_with(|| left_requested.cmp(right_requested)),
            (
                ScanDiagnostic::BudgetExceeded {
                    rel_path: left_path,
                    part: left_part,
                    source: left_source,
                },
                ScanDiagnostic::BudgetExceeded {
                    rel_path: right_path,
                    part: right_part,
                    source: right_source,
                },
            ) => left_path
                .cmp(right_path)
                .then_with(|| left_part.cmp(right_part))
                .then_with(|| compare_budget_errors(left_source, right_source)),
            (
                ScanDiagnostic::ChangedDuringRead {
                    rel_path: left_path,
                    part: left_part,
                    before: left_before,
                    after: left_after,
                },
                ScanDiagnostic::ChangedDuringRead {
                    rel_path: right_path,
                    part: right_part,
                    before: right_before,
                    after: right_after,
                },
            ) => left_path
                .cmp(right_path)
                .then_with(|| left_part.cmp(right_part))
                .then_with(|| left_before.cmp(right_before))
                .then_with(|| left_after.cmp(right_after)),
            (
                ScanDiagnostic::DigestFailed {
                    rel_path: left_path,
                    message: left_message,
                },
                ScanDiagnostic::DigestFailed {
                    rel_path: right_path,
                    message: right_message,
                },
            ) => left_path
                .cmp(right_path)
                .then_with(|| left_message.cmp(right_message)),
            (
                ScanDiagnostic::MalformedGuid {
                    rel_path: left_path,
                },
                ScanDiagnostic::MalformedGuid {
                    rel_path: right_path,
                },
            ) => left_path.cmp(right_path),
            (
                ScanDiagnostic::PayloadNotRetained {
                    rel_path: left_path,
                    length: left_length,
                    retained_limit: left_limit,
                },
                ScanDiagnostic::PayloadNotRetained {
                    rel_path: right_path,
                    length: right_length,
                    retained_limit: right_limit,
                },
            ) => left_path
                .cmp(right_path)
                .then_with(|| left_length.cmp(right_length))
                .then_with(|| left_limit.cmp(right_limit)),
            _ => Ordering::Equal,
        })
}

fn diagnostic_rank(diagnostic: &ScanDiagnostic) -> u8 {
    match diagnostic {
        ScanDiagnostic::WalkFailed { .. } => 0,
        ScanDiagnostic::PathRejected { .. } => 1,
        ScanDiagnostic::ReadFailed { .. } => 2,
        ScanDiagnostic::LimitExceeded { .. } => 3,
        ScanDiagnostic::AllocationFailed { .. } => 4,
        ScanDiagnostic::BudgetExceeded { .. } => 5,
        ScanDiagnostic::ChangedDuringRead { .. } => 6,
        ScanDiagnostic::DigestFailed { .. } => 7,
        ScanDiagnostic::MalformedGuid { .. } => 8,
        ScanDiagnostic::PayloadNotRetained { .. } => 9,
    }
}

fn compare_budget_errors(left: &BudgetError, right: &BudgetError) -> Ordering {
    budget_error_rank(left)
        .cmp(&budget_error_rank(right))
        .then_with(|| match (left, right) {
            (
                BudgetError::InvalidLimit {
                    resource: left_resource,
                },
                BudgetError::InvalidLimit {
                    resource: right_resource,
                },
            )
            | (
                BudgetError::ArithmeticOverflow {
                    resource: left_resource,
                },
                BudgetError::ArithmeticOverflow {
                    resource: right_resource,
                },
            )
            | (
                BudgetError::DomainMismatch {
                    resource: left_resource,
                },
                BudgetError::DomainMismatch {
                    resource: right_resource,
                },
            ) => left_resource.cmp(right_resource),
            (
                BudgetError::Exceeded {
                    resource: left_resource,
                    limit: left_limit,
                    requested: left_requested,
                },
                BudgetError::Exceeded {
                    resource: right_resource,
                    limit: right_limit,
                    requested: right_requested,
                },
            ) => left_resource
                .cmp(right_resource)
                .then_with(|| left_limit.cmp(right_limit))
                .then_with(|| left_requested.cmp(right_requested)),
            (
                BudgetError::ExpansionRatioExceeded {
                    compressed_bytes: left_compressed,
                    decompressed_bytes: left_decompressed,
                    max_ratio: left_ratio,
                },
                BudgetError::ExpansionRatioExceeded {
                    compressed_bytes: right_compressed,
                    decompressed_bytes: right_decompressed,
                    max_ratio: right_ratio,
                },
            ) => left_compressed
                .cmp(right_compressed)
                .then_with(|| left_decompressed.cmp(right_decompressed))
                .then_with(|| left_ratio.cmp(right_ratio)),
            _ => Ordering::Equal,
        })
}

fn budget_error_rank(error: &BudgetError) -> u8 {
    match error {
        BudgetError::InvalidLimit { .. } => 0,
        BudgetError::ArithmeticOverflow { .. } => 1,
        BudgetError::DomainMismatch { .. } => 2,
        BudgetError::Exceeded { .. } => 3,
        BudgetError::ExpansionRatioExceeded { .. } => 4,
    }
}

fn retained_meta_relative_path(
    asset_relative: &Path,
    budget: &mut AssetLoadBudget,
) -> std::result::Result<PathBuf, ReadFailure> {
    let Some(requested) = asset_relative.as_os_str().len().checked_add(".meta".len()) else {
        return Err(ReadFailure::Allocation {
            part: SourcePart::Meta,
            requested: u64::MAX,
        });
    };
    let Some(capacity_bound) = meta_relative_path_capacity_bound(asset_relative) else {
        return Err(ReadFailure::Allocation {
            part: SourcePart::Meta,
            requested: u64::MAX,
        });
    };
    let capacity_bound_u64 =
        u64::try_from(capacity_bound).map_err(|_| ReadFailure::Allocation {
            part: SourcePart::Meta,
            requested: u64::MAX,
        })?;
    budget
        .consume_bytes(capacity_bound_u64)
        .map_err(|source| ReadFailure::Budget {
            part: SourcePart::Meta,
            source,
        })?;

    let mut meta_relative = PathBuf::new();
    meta_relative
        .try_reserve_exact(requested)
        .map_err(|_| ReadFailure::Allocation {
            part: SourcePart::Meta,
            requested: u64::try_from(requested).unwrap_or(u64::MAX),
        })?;
    if meta_relative.capacity() > capacity_bound {
        return Err(ReadFailure::Allocation {
            part: SourcePart::Meta,
            requested: u64::try_from(meta_relative.capacity()).unwrap_or(u64::MAX),
        });
    }
    meta_relative.push(asset_relative);
    meta_relative.as_mut_os_string().push(".meta");
    Ok(meta_relative)
}

fn meta_relative_path_capacity_bound(asset_relative: &Path) -> Option<usize> {
    asset_relative
        .as_os_str()
        .len()
        .checked_add(".meta".len())?
        .checked_add(READ_META_PATH_EXTRA_CAPACITY)
}

fn configured_relative_path(project_root: &Path, path: &Path) -> Result<PathBuf> {
    configured_relative_path_if_inside(project_root, path)?.ok_or_else(|| {
        anyhow!(
            "configured path must remain inside project root: {}",
            path.display()
        )
    })
}

fn configured_relative_path_if_inside(project_root: &Path, path: &Path) -> Result<Option<PathBuf>> {
    let absolute = if path.is_absolute() {
        normalize_absolute(path)?
    } else {
        normalize_absolute(&project_root.join(path))?
    };
    let Ok(relative) = strip_project_root(project_root, &absolute) else {
        return Ok(None);
    };
    if !relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
        && !relative.as_os_str().is_empty()
    {
        return Err(anyhow!(
            "configured path is not a normalized project-relative path: {}",
            path.display()
        ));
    }
    Ok(Some(relative.to_path_buf()))
}

fn relative_path_to_portable(path: &Path) -> Option<String> {
    if path.as_os_str().is_empty() {
        return Some(String::new());
    }
    let mut output = String::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return None;
        };
        let component = component.to_str()?;
        if !is_portable_component(component) {
            return None;
        }
        if !output.is_empty() {
            output.push('/');
        }
        output.push_str(component);
    }
    Some(output)
}

fn path_depth(path: &Path) -> std::result::Result<u32, ScanError> {
    u32::try_from(path.components().count()).map_err(|_| {
        ScanError::Budget(BudgetError::ArithmeticOverflow {
            resource: "scan path depth",
        })
    })
}

fn normalize_relative_prefixes(paths: &mut Vec<String>) {
    sort_and_dedup_portable_paths(paths);
    let mut retained = 0;
    for candidate in 0..paths.len() {
        let covered = retained > 0 && is_path_at_or_below(&paths[candidate], &paths[retained - 1]);
        if covered {
            continue;
        }
        paths.swap(retained, candidate);
        retained += 1;
    }
    paths.truncate(retained);
}

fn sort_and_dedup_portable_paths(paths: &mut Vec<String>) {
    paths.sort_unstable_by(|left, right| compare_portable_paths(left, right));
    paths.dedup_by(|left, right| portable_paths_equal(left, right));
}

fn normalize_relative_scan_roots(paths: &mut Vec<PathBuf>) {
    paths.sort_unstable_by(|left, right| compare_relative_paths(left, right));
    paths.dedup_by(|left, right| relative_paths_equal(left, right));
    let mut retained = 0;
    for candidate in 0..paths.len() {
        let covered = retained > 0
            && (paths[retained - 1].as_os_str().is_empty()
                || relative_path_starts_with(&paths[candidate], &paths[retained - 1]));
        if covered {
            continue;
        }
        paths.swap(retained, candidate);
        retained += 1;
    }
    paths.truncate(retained);
}

#[cfg(not(windows))]
fn observed_component_matches(configured: &OsStr, observed: &OsStr) -> bool {
    configured == observed
}

#[cfg(windows)]
fn observed_component_matches(configured: &OsStr, observed: &OsStr) -> bool {
    windows_path_component_eq(configured, observed)
}

fn is_anchored_type_mismatch(error: &AnchoredFsError) -> bool {
    matches!(
        error,
        AnchoredFsError::NotDirectory | AnchoredFsError::NotRegular
    )
}

fn open_child_entry(
    parent: &ReadDirectory,
    name: &OsStr,
    hint: EntryKindHint,
) -> std::result::Result<OpenedEntry, AnchoredFsError> {
    let directory_first = !matches!(hint, EntryKindHint::RegularFile);
    if directory_first {
        match parent.open_directory(name) {
            Ok(directory) => return Ok(OpenedEntry::Directory(directory)),
            Err(error) if !is_anchored_type_mismatch(&error) => return Err(error),
            Err(_) => {}
        }
        match parent.open_regular(name) {
            Ok(file) => Ok(OpenedEntry::File(file)),
            Err(error) if is_anchored_type_mismatch(&error) => Ok(OpenedEntry::Other),
            Err(error) => Err(error),
        }
    } else {
        match parent.open_regular(name) {
            Ok(file) => return Ok(OpenedEntry::File(file)),
            Err(error) if !is_anchored_type_mismatch(&error) => return Err(error),
            Err(_) => {}
        }
        match parent.open_directory(name) {
            Ok(directory) => Ok(OpenedEntry::Directory(directory)),
            Err(error) if is_anchored_type_mismatch(&error) => Ok(OpenedEntry::Other),
            Err(error) => Err(error),
        }
    }
}

fn is_explicit_unity_exclusion(relative: &Path) -> bool {
    let Some(Component::Normal(root)) = relative.components().next() else {
        return false;
    };
    let Some(root) = root.to_str() else {
        return false;
    };
    #[cfg(windows)]
    {
        [".git", "Library", "Temp", "Obj", "Logs"]
            .iter()
            .any(|excluded| root.eq_ignore_ascii_case(excluded))
    }
    #[cfg(not(windows))]
    {
        matches!(root, ".git" | "Library" | "Temp" | "Obj" | "Logs")
    }
}

#[cfg(not(windows))]
fn relative_path_starts_with(path: &Path, prefix: &Path) -> bool {
    path.starts_with(prefix)
}

#[cfg(not(windows))]
fn compare_relative_paths(left: &Path, right: &Path) -> Ordering {
    left.cmp(right)
}

#[cfg(not(windows))]
fn relative_paths_equal(left: &Path, right: &Path) -> bool {
    left == right
}

#[cfg(windows)]
fn relative_path_starts_with(path: &Path, prefix: &Path) -> bool {
    let mut path_components = path.components();
    for expected in prefix.components() {
        let (Component::Normal(expected), Some(Component::Normal(actual))) =
            (expected, path_components.next())
        else {
            return false;
        };
        if !windows_path_component_eq(expected, actual) {
            return false;
        }
    }
    true
}

#[cfg(windows)]
fn compare_relative_paths(left: &Path, right: &Path) -> Ordering {
    let mut left_components = left.components();
    let mut right_components = right.components();
    loop {
        match (left_components.next(), right_components.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(Component::Normal(left)), Some(Component::Normal(right))) => {
                let ordering = windows_path_component_cmp(left, right);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            _ => return left.cmp(right),
        }
    }
}

#[cfg(windows)]
fn relative_paths_equal(left: &Path, right: &Path) -> bool {
    compare_relative_paths(left, right) == Ordering::Equal
}

fn is_search_ignore_policy_path(path: &str) -> bool {
    if path.contains('/') {
        return false;
    }
    crate::is_search_ignore_v1_file_name(OsStr::new(path))
}

fn normalize_absolute(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(anyhow!("path must be absolute: {}", path.display()));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(anyhow!(
                        "path escapes its filesystem root: {}",
                        path.display()
                    ));
                }
            }
            Component::Normal(component) => normalized.push(component),
        }
    }
    if !normalized.is_absolute() {
        return Err(anyhow!(
            "path normalization lost its root: {}",
            path.display()
        ));
    }
    Ok(normalized)
}

fn is_path_at_or_below(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let mut path_components = path.split('/');
    for expected in prefix.split('/') {
        let Some(actual) = path_components.next() else {
            return false;
        };
        if !portable_path_component_eq(actual, expected) {
            return false;
        }
    }
    true
}

fn portable_paths_equal(left: &str, right: &str) -> bool {
    let mut left_components = left.split('/');
    let mut right_components = right.split('/');
    loop {
        match (left_components.next(), right_components.next()) {
            (None, None) => return true,
            (Some(left), Some(right)) if portable_path_component_eq(left, right) => {}
            _ => return false,
        }
    }
}

fn sorted_paths_contain(paths: &[String], path: &str) -> bool {
    paths
        .binary_search_by(|candidate| compare_portable_paths(candidate, path))
        .is_ok()
}

#[cfg(test)]
fn sorted_paths_get<'paths>(paths: &'paths [String], path: &str) -> Option<&'paths str> {
    paths
        .binary_search_by(|candidate| compare_portable_paths(candidate, path))
        .ok()
        .map(|index| paths[index].as_str())
}

fn known_paths_contain(paths: &[ProjectSourcePath], path: &str) -> bool {
    paths
        .binary_search_by(|candidate| compare_portable_paths(candidate.relative_path(), path))
        .is_ok()
}

fn known_paths_get<'paths>(
    paths: &'paths [ProjectSourcePath],
    path: &str,
) -> Option<&'paths ProjectSourcePath> {
    paths
        .binary_search_by(|candidate| compare_portable_paths(candidate.relative_path(), path))
        .ok()
        .and_then(|index| paths.get(index))
}

fn sorted_paths_have_descendant(paths: &[String], prefix: &str) -> bool {
    let candidate =
        paths.partition_point(|path| compare_portable_paths(path, prefix) == Ordering::Less);
    paths
        .get(candidate)
        .is_some_and(|path| is_path_at_or_below(path, prefix))
}

#[cfg(test)]
fn sorted_paths_have_strict_descendant(paths: &[String], prefix: &str) -> bool {
    let range = sorted_path_descendant_range(paths, prefix);
    paths[range]
        .iter()
        .any(|path| !portable_paths_equal(path, prefix))
}

fn known_paths_have_strict_descendant(paths: &[ProjectSourcePath], prefix: &str) -> bool {
    let range = known_path_descendant_range(paths, prefix);
    paths[range]
        .iter()
        .any(|path| !portable_paths_equal(path.relative_path(), prefix))
}

fn known_path_descendant_range(
    paths: &[ProjectSourcePath],
    prefix: &str,
) -> std::ops::Range<usize> {
    let start =
        paths.partition_point(|path| compare_portable_paths(path.relative_path(), prefix).is_lt());
    let end = start
        + paths[start..].partition_point(|path| is_path_at_or_below(path.relative_path(), prefix));
    start..end
}

#[cfg(test)]
fn sorted_path_descendant_range(paths: &[String], prefix: &str) -> std::ops::Range<usize> {
    let start = paths.partition_point(|path| compare_portable_paths(path, prefix).is_lt());
    let end = start + paths[start..].partition_point(|path| is_path_at_or_below(path, prefix));
    start..end
}

fn sorted_prefixes_contain(prefixes: &[String], path: &str) -> bool {
    let after_candidate = prefixes
        .partition_point(|prefix| compare_portable_paths(prefix, path) != Ordering::Greater);
    after_candidate
        .checked_sub(1)
        .and_then(|candidate| prefixes.get(candidate))
        .is_some_and(|prefix| is_path_at_or_below(path, prefix))
}

#[cfg(not(windows))]
fn portable_path_component_eq(left: &str, right: &str) -> bool {
    left == right
}

#[cfg(windows)]
fn portable_path_component_eq(left: &str, right: &str) -> bool {
    windows_path_component_eq(OsStr::new(left), OsStr::new(right))
}

fn is_supported_asset_path(path: &Path) -> bool {
    !is_meta_path(path) && !is_hidden_file(path) && path.file_name().is_some()
}

fn is_hidden_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

fn is_meta_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("meta"))
}

#[cfg(test)]
fn append_meta_extension(asset_path: &Path) -> PathBuf {
    let mut meta_path = asset_path.to_path_buf();
    if let Some(file_name) = meta_path.file_name() {
        let mut meta_name = file_name.to_os_string();
        meta_name.push(".meta");
        meta_path.set_file_name(meta_name);
    }
    meta_path
}

fn asset_path_from_meta(meta_path: &Path) -> Option<PathBuf> {
    let file_name = meta_path.file_name()?.to_str()?;
    let suffix_start = file_name.len().checked_sub(".meta".len())?;
    let (asset_name, suffix) = file_name.split_at(suffix_start);
    if !suffix.eq_ignore_ascii_case(".meta") {
        return None;
    }
    if asset_name.is_empty() {
        return None;
    }
    let mut asset_path = meta_path.to_path_buf();
    asset_path.set_file_name(asset_name);
    Some(asset_path)
}

fn classify_kind(path: &Path) -> SearchKind {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "prefab" => SearchKind::Prefab,
        "unity" => SearchKind::Scene,
        "mat" => SearchKind::Material,
        "cs" => SearchKind::Script,
        "anim" => SearchKind::AnimationClip,
        "controller" => SearchKind::AnimatorController,
        "asset" => SearchKind::Asset,
        "shader" => SearchKind::Shader,
        "png" | "jpg" | "jpeg" | "tga" | "psd" => SearchKind::Texture,
        "wav" | "mp3" | "ogg" => SearchKind::Audio,
        _ => SearchKind::File,
    }
}

fn saturating_usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use tempfile::TempDir;
    use unity_asset_core::AssetLoadLimits;

    fn complete_identity(bytes: &[u8]) -> SourceIdentityPart {
        SourceIdentityPart {
            digest: Some(DigestV1::hash_bytes(bytes)),
            hint: FileHint {
                size: u64::try_from(bytes.len()).unwrap(),
                mtime_ms: None,
            },
        }
    }

    fn scanner(project: &TempDir, limits: ScanReadLimits) -> ProjectScanner {
        scanner_with_options(project, SearchIndexOptions::default(), limits)
    }

    fn scanner_with_options(
        project: &TempDir,
        options: SearchIndexOptions,
        limits: ScanReadLimits,
    ) -> ProjectScanner {
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        let paths = IndexPaths::for_project(project.path().to_path_buf(), None, None).unwrap();
        ProjectScanner::new(&paths, options, limits).unwrap()
    }

    fn plan_with_default_budget(
        scanner: &ProjectScanner,
        intent: ScanIntent,
        known: &[String],
    ) -> ScanPlan {
        let known = project_source_paths(scanner, known);
        scanner
            .plan(intent, &known, &mut AssetLoadBudget::default())
            .unwrap()
    }

    fn project_source_path(scanner: &ProjectScanner, relative_path: &str) -> ProjectSourcePath {
        let path = scanner
            .project
            .path_space()
            .resolve(Path::new(relative_path))
            .unwrap()
            .unwrap();
        ProjectSourcePath::from_project_path(&path, relative_path.to_owned())
    }

    fn project_source_paths(
        scanner: &ProjectScanner,
        relative_paths: &[String],
    ) -> Vec<ProjectSourcePath> {
        let mut paths = relative_paths
            .iter()
            .map(|path| project_source_path(scanner, path))
            .collect::<Vec<_>>();
        paths.sort_unstable_by(|left, right| {
            compare_portable_paths(left.relative_path(), right.relative_path())
        });
        paths
    }

    fn deleted_paths(plan: &ScanPlan) -> Vec<&str> {
        plan.deleted
            .iter()
            .map(ProjectSourcePath::relative_path)
            .collect()
    }

    fn candidate_facts(plan: &ScanPlan) -> Vec<(&str, &str, SearchKind)> {
        plan.present
            .iter()
            .map(|candidate| {
                (
                    candidate.relative_path(),
                    candidate.name.as_str(),
                    candidate.kind,
                )
            })
            .collect()
    }

    fn changed_paths<I, P>(scanner: &ProjectScanner, paths: I) -> ScanIntent
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        ScanIntent::ChangedPaths(scanner.project.path_space().resolve_set(paths).unwrap())
    }

    fn traversal_limit_intent(scanner: &ProjectScanner) -> ScanIntent {
        changed_paths(
            scanner,
            [
                PathBuf::from("Assets"),
                PathBuf::from("Assets/.first.asset"),
                PathBuf::from("Assets/.Second.asset"),
            ],
        )
    }

    #[test]
    fn selected_scope_queries_sorted_exact_paths_and_prefixes() {
        let mut exact_files = vec![
            "Assets/Z.asset".to_owned(),
            "Assets/Characters/Hero.prefab".to_owned(),
        ];
        sort_and_dedup_portable_paths(&mut exact_files);
        let mut rescan_dirs = vec!["Packages/Feature".to_owned(), "Assets/Scenes".to_owned()];
        normalize_relative_prefixes(&mut rescan_dirs);
        let scope = DiscoveryScope::Selected {
            exact_files: &exact_files,
            rescan_dirs: &rescan_dirs,
        };

        assert!(scope.should_visit_directory("Assets"));
        assert!(scope.should_visit_directory("Assets/Characters"));
        assert!(scope.should_visit_directory("Assets/Scenes/Nested"));
        assert!(scope.includes_file("Assets/Characters/Hero.prefab"));
        assert!(scope.includes_file("Assets/Scenes/Nested/Level.unity"));
        assert!(!scope.should_visit_directory("Library"));
        assert!(!scope.includes_file("Assets/Unrelated.asset"));
    }

    #[test]
    fn candidate_lookup_matches_the_primary_portable_sort_order() {
        let project = tempfile::tempdir().unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let mut candidates = vec![
            ScanCandidate::new(
                project_source_path(&scanner, "Assets/Z.asset"),
                "Z".to_owned(),
                SearchKind::Asset,
            ),
            ScanCandidate::new(
                project_source_path(&scanner, "Assets/A.asset"),
                "A".to_owned(),
                SearchKind::Asset,
            ),
        ];
        sort_and_dedup_candidates(&mut candidates);

        assert!(contains_candidate(
            &candidates,
            &project_source_path(&scanner, "Assets/A.asset")
        ));
        assert!(contains_candidate(
            &candidates,
            &project_source_path(&scanner, "Assets/Z.asset")
        ));
        assert!(!contains_candidate(
            &candidates,
            &project_source_path(&scanner, "Assets/Missing.asset")
        ));
    }

    #[test]
    fn scanner_rejects_a_project_path_rebound_after_index_derivation() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        let original = temporary.path().join("original-project");
        fs::create_dir_all(project.join("Assets")).unwrap();
        let paths = IndexPaths::for_project(project.clone(), None, None).unwrap();
        fs::rename(&project, &original).unwrap();
        fs::create_dir_all(project.join("Assets")).unwrap();

        let error = ProjectScanner::new(
            &paths,
            SearchIndexOptions::default(),
            ScanReadLimits::default(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("revalidate retained project authority")
        );
    }

    #[test]
    fn scanner_keeps_its_project_binding_across_root_directory_changes() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::write(project.path().join("Assets/Visible.asset"), b"asset").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());

        fs::write(project.path().join("README.md"), b"project notes").unwrap();

        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
        assert_eq!(plan.present[0].relative_path(), "Assets/Visible.asset");
    }

    #[test]
    fn creating_the_root_policy_after_scanner_construction_triggers_a_valid_full_scan() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::write(project.path().join("Assets/Hidden.asset"), b"asset").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let before = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
        let known = before
            .present
            .iter()
            .map(|candidate| candidate.relative_path().to_owned())
            .collect::<Vec<_>>();
        let policy = project.path().join(SEARCH_IGNORE_V1_FILE);

        fs::write(&policy, b"exact:Assets/Hidden.asset\n").unwrap();

        let intent = changed_paths(&scanner, [policy]);
        let changed = plan_with_default_budget(&scanner, intent, &known);
        assert!(changed.present.is_empty());
        assert_eq!(deleted_paths(&changed), ["Assets/Hidden.asset"]);
    }

    fn exact_traversal_limits(plan: &ScanPlan) -> crate::ScanTraversalLimits {
        let usage = plan.traversal_usage;
        crate::ScanTraversalLimits {
            max_entries: usage.entries,
            max_path_bytes: usage.path_bytes,
            max_depth: usage.max_depth,
            max_directories: usage.directories,
            max_files: usage.files,
            max_diagnostics: usage.diagnostics,
            max_policy_matches: crate::ScanTraversalLimits::default().max_policy_matches,
        }
    }

    fn diagnostic_retained_bytes(diagnostics: &Vec<ScanDiagnostic>) -> u64 {
        let dynamic = diagnostics.iter().fold(0_u64, |total, diagnostic| {
            total + diagnostic_backing_bytes(diagnostic).unwrap()
        });
        dynamic + checked_vec_bytes::<ScanDiagnostic>(diagnostics.capacity()).unwrap()
    }

    fn read_source_non_payload_retained_bytes(
        candidate: &ScanCandidate,
        source: &ReadSource,
        diagnostics: &Vec<ScanDiagnostic>,
    ) -> u64 {
        path_backing_bytes(&source.abs_path).unwrap()
            + u64::try_from(
                meta_relative_path_capacity_bound(Path::new(candidate.relative_path())).unwrap(),
            )
            .unwrap()
            + u64::try_from(std::mem::size_of::<ReadSource>()).unwrap()
            + string_backing_bytes(&source.rel_path).unwrap()
            + string_backing_bytes(&source.name).unwrap()
            + source
                .guid
                .as_ref()
                .map_or(0, |guid| string_backing_bytes(guid).unwrap())
            + diagnostic_retained_bytes(diagnostics)
    }

    #[test]
    fn full_plan_is_sorted_and_reports_stale_known_paths() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets/Z")).unwrap();
        fs::write(project.path().join("Assets/Z/B.asset"), b"b").unwrap();
        fs::write(project.path().join("Assets/A.asset"), b"a").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let known = [
            "Assets/A.asset".to_owned(),
            "Assets/Removed.asset".to_owned(),
        ];

        let plan = plan_with_default_budget(&scanner, ScanIntent::Reconcile, &known);

        assert_eq!(
            plan.present
                .iter()
                .map(ScanCandidate::relative_path)
                .collect::<Vec<_>>(),
            ["Assets/A.asset", "Assets/Z/B.asset"]
        );
        assert_eq!(deleted_paths(&plan), ["Assets/Removed.asset"]);
        assert_eq!(plan.metrics.discovered, 2);
        assert_eq!(plan.metrics.deleted, 1);
    }

    #[test]
    fn sorted_known_path_queries_use_portable_component_order() {
        let mut paths = vec![
            "Assets/Dir-Z.asset".to_owned(),
            "Assets/Dir/Nested/B.asset".to_owned(),
            "Assets/Dir/A.asset".to_owned(),
            "Assets/Exact.asset".to_owned(),
        ];
        sort_and_dedup_portable_paths(&mut paths);

        assert_eq!(
            sorted_paths_get(&paths, "Assets/Exact.asset"),
            Some("Assets/Exact.asset")
        );
        assert!(sorted_paths_have_strict_descendant(&paths, "Assets/Dir"));
        assert!(!sorted_paths_have_strict_descendant(
            &paths,
            "Assets/Exact.asset"
        ));
        assert_eq!(
            &paths[sorted_path_descendant_range(&paths, "Assets/Dir")],
            [
                "Assets/Dir/A.asset".to_owned(),
                "Assets/Dir/Nested/B.asset".to_owned(),
            ]
        );
    }

    #[test]
    fn full_plan_is_independent_of_filesystem_creation_order() {
        let forward_project = tempfile::tempdir().unwrap();
        let reverse_project = tempfile::tempdir().unwrap();
        let assets = [
            "Assets/Z/Last.asset",
            "Assets/A/First.prefab",
            "Assets/Middle.mat",
            "Packages/com.example/Runtime.asset",
        ];
        for relative in assets {
            let path = forward_project.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, relative.as_bytes()).unwrap();
        }
        for relative in assets.into_iter().rev() {
            let path = reverse_project.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, relative.as_bytes()).unwrap();
        }

        let forward_plan = plan_with_default_budget(
            &scanner(&forward_project, ScanReadLimits::default()),
            ScanIntent::Full,
            &[],
        );
        let reverse_plan = plan_with_default_budget(
            &scanner(&reverse_project, ScanReadLimits::default()),
            ScanIntent::Full,
            &[],
        );

        assert_eq!(
            candidate_facts(&forward_plan),
            candidate_facts(&reverse_plan)
        );
        assert_eq!(forward_plan.deleted, reverse_plan.deleted);
        assert_eq!(forward_plan.metrics, reverse_plan.metrics);
        assert_eq!(forward_plan.traversal_usage, reverse_plan.traversal_usage);

        let path_limit = forward_plan.traversal_usage.path_bytes - 1;
        let limited_evidence = |project: &TempDir| {
            let options = SearchIndexOptions {
                scan_limits: crate::ScanTraversalLimits {
                    max_path_bytes: path_limit,
                    ..crate::ScanTraversalLimits::default()
                },
                ..SearchIndexOptions::default()
            };
            let error = scanner_with_options(project, options, ScanReadLimits::default())
                .plan(ScanIntent::Full, &[], &mut AssetLoadBudget::default())
                .unwrap_err();
            match error {
                ScanError::TraversalLimitExceeded {
                    resource: ScanLimitResource::PathBytes,
                    observed_at_least,
                    limit,
                } => (observed_at_least, limit),
                other => panic!("unexpected reverse-order budget error: {other}"),
            }
        };
        assert_eq!(
            limited_evidence(&forward_project),
            limited_evidence(&reverse_project)
        );
    }

    #[test]
    fn path_limit_stops_at_the_first_over_budget_entry() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        for name in [
            "a.asset",
            "medium-length-name.prefab",
            "very-long-directory-entry-name-for-budget-evidence.mat",
        ] {
            fs::write(project.path().join("Assets").join(name), b"asset").unwrap();
        }
        let baseline = plan_with_default_budget(
            &scanner(&project, ScanReadLimits::default()),
            ScanIntent::Full,
            &[],
        );
        let complete_path_bytes = baseline.traversal_usage.path_bytes;
        let options = SearchIndexOptions {
            scan_limits: crate::ScanTraversalLimits {
                max_path_bytes: 1,
                ..crate::ScanTraversalLimits::default()
            },
            ..SearchIndexOptions::default()
        };
        let limited = scanner_with_options(&project, options, ScanReadLimits::default());

        let error = limited
            .plan(ScanIntent::Full, &[], &mut AssetLoadBudget::default())
            .unwrap_err();

        assert!(matches!(
            error,
            ScanError::TraversalLimitExceeded {
                resource: ScanLimitResource::PathBytes,
                observed_at_least,
                limit: 1,
            } if observed_at_least == 2 && observed_at_least < complete_path_bytes
        ));
    }

    #[test]
    fn directory_entries_are_sorted_before_traversal() {
        let project = tempfile::tempdir().unwrap();
        for name in ["z-last.asset", "a-first.asset", "m-middle.asset"] {
            fs::write(project.path().join(name), b"asset").unwrap();
        }
        let directory = ReadDirectory::open(project.path(), OpenPolicy::ProjectSource).unwrap();
        let mut entries = directory
            .entries()
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        entries.sort_unstable_by(|left, right| right.name().cmp(left.name()));

        sort_directory_entries(&mut entries);

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name().to_str().unwrap())
                .collect::<Vec<_>>(),
            ["a-first.asset", "m-middle.asset", "z-last.asset"]
        );
    }

    #[cfg(windows)]
    #[test]
    fn candidate_deduplication_uses_windows_path_equivalence() {
        let project = tempfile::tempdir().unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let mut candidates = vec![
            ScanCandidate::new(
                project_source_path(&scanner, "Assets/Owner.prefab"),
                "Owner".to_owned(),
                SearchKind::Prefab,
            ),
            ScanCandidate::new(
                project_source_path(&scanner, "ASSETS/OWNER.prefab"),
                "OWNER".to_owned(),
                SearchKind::Prefab,
            ),
        ];

        sort_and_dedup_candidates(&mut candidates);

        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn project_scanner_enforces_every_traversal_limit_at_its_exact_usage() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets/Deep/Nested")).unwrap();
        fs::write(project.path().join("Assets/First.asset"), b"first").unwrap();
        fs::write(
            project.path().join("Assets/Deep/Nested/Second.prefab"),
            b"second",
        )
        .unwrap();
        fs::write(project.path().join("Assets/.first.asset"), b"hidden").unwrap();
        fs::write(project.path().join("Assets/.Second.asset"), b"hidden").unwrap();
        let baseline_scanner = scanner(&project, ScanReadLimits::default());
        let baseline = plan_with_default_budget(
            &baseline_scanner,
            traversal_limit_intent(&baseline_scanner),
            &[],
        );
        let usage = baseline.traversal_usage;
        assert!(usage.entries > 1);
        assert!(usage.path_bytes > 1);
        assert!(usage.max_depth > 1);
        assert!(usage.directories > 1);
        assert!(usage.files > 1);
        assert!(usage.diagnostics > 1);
        let exact = exact_traversal_limits(&baseline);
        let exact_options = SearchIndexOptions {
            scan_limits: exact,
            ..SearchIndexOptions::default()
        };
        exact_options.validate().unwrap();

        let exact_scanner =
            scanner_with_options(&project, exact_options, ScanReadLimits::default());
        let exact_plan =
            plan_with_default_budget(&exact_scanner, traversal_limit_intent(&exact_scanner), &[]);

        assert_eq!(exact_plan.traversal_usage, usage);
    }

    #[test]
    fn project_scanner_rejects_one_under_each_traversal_limit_without_a_plan() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets/Deep/Nested")).unwrap();
        fs::write(project.path().join("Assets/First.asset"), b"first").unwrap();
        fs::write(
            project.path().join("Assets/Deep/Nested/Second.prefab"),
            b"second",
        )
        .unwrap();
        fs::write(project.path().join("Assets/.first.asset"), b"hidden").unwrap();
        fs::write(project.path().join("Assets/.Second.asset"), b"hidden").unwrap();
        let baseline_scanner = scanner(&project, ScanReadLimits::default());
        let baseline = plan_with_default_budget(
            &baseline_scanner,
            traversal_limit_intent(&baseline_scanner),
            &[],
        );
        let exact = exact_traversal_limits(&baseline);
        let cases = [
            (
                ScanLimitResource::Entries,
                exact.max_entries,
                crate::ScanTraversalLimits {
                    max_entries: exact.max_entries - 1,
                    ..exact
                },
            ),
            (
                ScanLimitResource::PathBytes,
                exact.max_path_bytes,
                crate::ScanTraversalLimits {
                    max_path_bytes: exact.max_path_bytes - 1,
                    ..exact
                },
            ),
            (
                ScanLimitResource::Depth,
                u64::from(exact.max_depth),
                crate::ScanTraversalLimits {
                    max_depth: exact.max_depth - 1,
                    ..exact
                },
            ),
            (
                ScanLimitResource::Directories,
                exact.max_directories,
                crate::ScanTraversalLimits {
                    max_directories: exact.max_directories - 1,
                    ..exact
                },
            ),
            (
                ScanLimitResource::Files,
                exact.max_files,
                crate::ScanTraversalLimits {
                    max_files: exact.max_files - 1,
                    ..exact
                },
            ),
            (
                ScanLimitResource::Diagnostics,
                exact.max_diagnostics,
                crate::ScanTraversalLimits {
                    max_diagnostics: exact.max_diagnostics - 1,
                    ..exact
                },
            ),
        ];

        for (resource, observed, scan_limits) in cases {
            let options = SearchIndexOptions {
                scan_limits,
                ..SearchIndexOptions::default()
            };
            options.validate().unwrap();
            let scanner = scanner_with_options(&project, options, ScanReadLimits::default());
            let result = scanner.plan(
                traversal_limit_intent(&scanner),
                &[],
                &mut AssetLoadBudget::default(),
            );

            match result {
                Err(ScanError::TraversalLimitExceeded {
                    resource: actual_resource,
                    observed_at_least,
                    limit,
                }) => {
                    assert_eq!(actual_resource, resource);
                    assert_eq!(observed_at_least, observed);
                    assert_eq!(limit, observed - 1);
                }
                unexpected => panic!("unexpected result for {resource:?}: {unexpected:?}"),
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn joined_path_uses_native_separators_below_a_verbatim_root() {
        let path = prepared_joined_path(
            Path::new(r"\\?\C:\Project"),
            Path::new("Assets/owner.prefab"),
            &mut AssetLoadBudget::default(),
            "verbatim path test",
        )
        .unwrap();

        assert_eq!(path, PathBuf::from(r"\\?\C:\Project\Assets\owner.prefab"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn non_utf8_entry_is_diagnosed_without_becoming_a_candidate() {
        use std::os::unix::ffi::OsStringExt as _;

        let project = tempfile::tempdir().unwrap();
        let assets = project.path().join("Assets");
        fs::create_dir_all(&assets).unwrap();
        let name = std::ffi::OsString::from_vec(b"invalid-\xFF.asset".to_vec());
        let path = assets.join(name);
        fs::write(&path, b"asset").unwrap();

        let plan = plan_with_default_budget(
            &scanner(&project, ScanReadLimits::default()),
            ScanIntent::Full,
            &[],
        );

        assert!(plan.present.is_empty());
        assert!(plan.diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic,
                ScanDiagnostic::PathRejected {
                    path: rejected,
                    reason: PathRejection::NonUtf8RelativePath,
                } if rejected == &path
            )
        }));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn non_portable_unix_file_names_are_diagnosed_without_failing_the_scan() {
        let project = tempfile::tempdir().unwrap();
        let assets = project.path().join("Assets");
        fs::create_dir_all(&assets).unwrap();
        for name in [r"contains\backslash.asset", "contains:colon.asset"] {
            fs::write(assets.join(name), b"asset").unwrap();
        }
        fs::write(assets.join("valid.asset"), b"asset").unwrap();

        let plan = plan_with_default_budget(
            &scanner(&project, ScanReadLimits::default()),
            ScanIntent::Full,
            &[],
        );

        assert_eq!(
            plan.present
                .iter()
                .map(ScanCandidate::relative_path)
                .collect::<Vec<_>>(),
            ["Assets/valid.asset"]
        );
        assert_eq!(
            plan.diagnostics
                .iter()
                .filter(|diagnostic| {
                    matches!(
                        diagnostic,
                        ScanDiagnostic::PathRejected {
                            reason: PathRejection::InvalidPath,
                            ..
                        }
                    )
                })
                .count(),
            2
        );
    }

    #[test]
    fn configured_scan_root_depth_is_enforced_before_empty_root_traversal() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        let options = SearchIndexOptions {
            scan_limits: crate::ScanTraversalLimits {
                max_depth: 0,
                ..SearchIndexOptions::default().scan_limits
            },
            ..SearchIndexOptions::default()
        };
        let scanner = scanner_with_options(&project, options, ScanReadLimits::default());

        assert!(matches!(
            scanner.plan(ScanIntent::Full, &[], &mut AssetLoadBudget::default()),
            Err(ScanError::TraversalLimitExceeded {
                resource: ScanLimitResource::Depth,
                observed_at_least: 1,
                limit: 0,
            })
        ));
    }

    #[cfg(windows)]
    #[test]
    fn changed_paths_use_windows_ordinal_case_matching_and_preserve_disk_case() {
        let project = tempfile::tempdir().unwrap();
        let asset = project.path().join("Assets/Owner.prefab");
        fs::create_dir_all(asset.parent().unwrap()).unwrap();
        fs::write(&asset, b"asset").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());

        let plan = plan_with_default_budget(
            &scanner,
            changed_paths(&scanner, [project.path().join("assets/owner.prefab")]),
            &[],
        );
        assert_eq!(
            plan.present
                .iter()
                .map(ScanCandidate::relative_path)
                .collect::<Vec<_>>(),
            ["Assets/Owner.prefab"]
        );

        fs::remove_file(&asset).unwrap();
        let known = ["Assets/Owner.prefab".to_owned()];
        let plan = plan_with_default_budget(
            &scanner,
            changed_paths(&scanner, [project.path().join("assets/owner.prefab")]),
            &known,
        );
        assert_eq!(deleted_paths(&plan), ["Assets/Owner.prefab"]);
    }

    #[cfg(windows)]
    #[test]
    fn configured_scan_root_case_rename_refreshes_observed_spelling() {
        let project = tempfile::tempdir().unwrap();
        let original_root = project.path().join("Assets/Characters");
        fs::create_dir_all(&original_root).unwrap();
        fs::write(original_root.join("Hero.prefab"), b"asset").unwrap();
        let paths = IndexPaths::for_project(
            project.path().to_path_buf(),
            None,
            Some(vec![original_root.clone()]),
        )
        .unwrap();
        let scanner = ProjectScanner::new(
            &paths,
            SearchIndexOptions::default(),
            ScanReadLimits::default(),
        )
        .unwrap();
        let before = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
        let coordinate = before.present[0].coordinate();
        assert_eq!(
            before.present[0].relative_path(),
            "Assets/Characters/Hero.prefab"
        );

        let intermediate_root = project.path().join("Assets/Characters-renaming");
        let observed_root = project.path().join("Assets/characters");
        fs::rename(&original_root, &intermediate_root).unwrap();
        fs::rename(&intermediate_root, &observed_root).unwrap();
        let known = ["Assets/Characters/Hero.prefab".to_owned()];
        for intent in [
            ScanIntent::Full,
            ScanIntent::Reconcile,
            changed_paths(&scanner, [observed_root.join("Hero.prefab")]),
        ] {
            let plan = plan_with_default_budget(&scanner, intent, &known);
            assert_eq!(plan.present.len(), 1);
            assert_eq!(
                plan.present[0].relative_path(),
                "Assets/characters/Hero.prefab"
            );
            assert_eq!(plan.present[0].coordinate(), coordinate);
            assert!(plan.deleted.is_empty());
        }

        let reopened = ProjectScanner::new(
            &paths,
            SearchIndexOptions::default(),
            ScanReadLimits::default(),
        )
        .unwrap();
        let plan = plan_with_default_budget(&reopened, ScanIntent::Full, &known);
        assert_eq!(
            plan.present[0].relative_path(),
            "Assets/characters/Hero.prefab"
        );
        assert_eq!(plan.present[0].coordinate(), coordinate);
        assert!(plan.deleted.is_empty());
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn plan_validation_rejects_a_replaced_directory_namespace() {
        let project = tempfile::tempdir().unwrap();
        let assets = project.path().join("Assets");
        let replacement = project.path().join("ReplacementAssets");
        let displaced = project.path().join("DisplacedAssets");
        fs::create_dir(&assets).unwrap();
        fs::create_dir(&replacement).unwrap();
        fs::write(assets.join("Original.asset"), b"original").unwrap();
        fs::write(replacement.join("Replacement.asset"), b"replacement").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        fs::rename(&assets, &displaced).unwrap();
        fs::rename(&replacement, &assets).unwrap();

        assert!(matches!(
            scanner.validate_plan(&plan),
            Err(ScanError::TraversalChangedDuringScan)
        ));
    }

    #[test]
    fn plan_validation_rejects_a_changed_search_policy() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir(project.path().join("Assets")).unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        fs::write(
            project.path().join(SEARCH_IGNORE_V1_FILE),
            b"subtree:Assets/Generated\n",
        )
        .unwrap();

        assert!(matches!(
            scanner.validate_plan(&plan),
            Err(ScanError::PolicyChangedDuringScan)
        ));
    }

    #[test]
    fn plan_validation_rejects_changed_asset_meta_and_absent_meta_proofs() {
        enum Mutation {
            Asset,
            Meta,
            MissingMeta,
        }

        for mutation in [Mutation::Asset, Mutation::Meta, Mutation::MissingMeta] {
            let project = tempfile::tempdir().unwrap();
            let assets = project.path().join("Assets");
            fs::create_dir_all(&assets).unwrap();
            let asset = assets.join("owner.prefab");
            let meta = assets.join("owner.prefab.meta");
            fs::write(&asset, b"asset-before").unwrap();
            if !matches!(mutation, Mutation::MissingMeta) {
                fs::write(
                    &meta,
                    b"fileFormatVersion: 2\nguid: 11111111111111111111111111111111\n",
                )
                .unwrap();
            }
            let scanner = scanner(&project, ScanReadLimits::default());
            let mut plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
            let mut budget = AssetLoadBudget::default();
            let outcome = scanner.read_source(&plan.present[0], None, &mut budget);
            let accepted = outcome
                .accepted
                .expect("successful scanner read must produce a source and proof");
            plan.record_source_proof(accepted.proof, &mut budget)
                .unwrap();

            match mutation {
                Mutation::Asset => fs::write(&asset, b"asset-after!").unwrap(),
                Mutation::Meta => fs::write(
                    &meta,
                    b"fileFormatVersion: 2\nguid: 21111111111111111111111111111111\n",
                )
                .unwrap(),
                Mutation::MissingMeta => fs::write(
                    &meta,
                    b"fileFormatVersion: 2\nguid: 11111111111111111111111111111111\n",
                )
                .unwrap(),
            }

            assert!(matches!(
                scanner.validate_source_proofs(&plan.validation.source_proofs),
                Err(ScanError::SourceChangedDuringScan)
            ));
        }
    }

    #[test]
    fn only_root_search_ignore_v1_rules_are_loaded() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets/Nested")).unwrap();
        fs::write(
            project.path().join(SEARCH_IGNORE_V1_FILE),
            b"glob:Assets/*.asset\n!exact:Assets/Keep.asset\n",
        )
        .unwrap();
        fs::write(
            project
                .path()
                .join("Assets/Nested/.unity-asset-search-ignore"),
            b"exact:Assets/Nested/Visible.asset\n",
        )
        .unwrap();
        for path in [
            "Assets/Drop.asset",
            "Assets/Keep.asset",
            "Assets/Nested/Visible.asset",
        ] {
            fs::write(project.path().join(path), b"asset").unwrap();
        }
        let scanner = scanner(&project, ScanReadLimits::default());

        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        assert_eq!(
            plan.present
                .iter()
                .map(ScanCandidate::relative_path)
                .collect::<Vec<_>>(),
            ["Assets/Keep.asset", "Assets/Nested/Visible.asset"]
        );
    }

    #[test]
    fn excluded_subtrees_descend_when_a_later_rule_reincludes_a_child() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets/Generated")).unwrap();
        fs::write(
            project.path().join(SEARCH_IGNORE_V1_FILE),
            b"subtree:Assets/Generated\n!exact:Assets/Generated/Keep.asset\n",
        )
        .unwrap();
        for path in ["Assets/Generated/Drop.asset", "Assets/Generated/Keep.asset"] {
            fs::write(project.path().join(path), b"asset").unwrap();
        }
        let scanner = scanner(&project, ScanReadLimits::default());

        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        assert_eq!(
            plan.present
                .iter()
                .map(ScanCandidate::relative_path)
                .collect::<Vec<_>>(),
            ["Assets/Generated/Keep.asset"]
        );
    }

    #[test]
    fn gitignore_and_ignore_files_are_inert() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::write(project.path().join(".gitignore"), b"Assets/Hidden.asset\n").unwrap();
        fs::write(project.path().join(".ignore"), b"Assets/Hidden.asset\n").unwrap();
        fs::write(project.path().join("Assets/Hidden.asset"), b"asset").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());

        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        assert_eq!(plan.present.len(), 1);
        assert_eq!(plan.present[0].relative_path(), "Assets/Hidden.asset");
    }

    #[test]
    fn search_ignore_file_and_line_limits_accept_exact_and_reject_one_over() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::write(project.path().join("Assets/Hidden.asset"), b"asset").unwrap();
        let encoded = b"exact:Assets/Hidden.asset\n";
        fs::write(project.path().join(SEARCH_IGNORE_V1_FILE), encoded).unwrap();
        let line_bytes = encoded.len() - 1;
        let exact_options = SearchIndexOptions {
            ignore_limits: crate::SearchIgnoreV1Limits {
                max_file_bytes: encoded.len() as u64,
                max_line_bytes: line_bytes,
                max_rules: 1,
                max_parser_work: (encoded.len() as u64) * 2,
                ..crate::SearchIgnoreV1Limits::default()
            },
            ..SearchIndexOptions::default()
        };
        let exact_scanner =
            scanner_with_options(&project, exact_options, ScanReadLimits::default());

        assert!(
            plan_with_default_budget(&exact_scanner, ScanIntent::Full, &[])
                .present
                .is_empty()
        );

        let file_error = scanner_with_options(
            &project,
            SearchIndexOptions {
                ignore_limits: crate::SearchIgnoreV1Limits {
                    max_file_bytes: (encoded.len() - 1) as u64,
                    ..exact_options.ignore_limits
                },
                ..exact_options
            },
            ScanReadLimits::default(),
        )
        .plan(ScanIntent::Full, &[], &mut AssetLoadBudget::default())
        .unwrap_err();
        assert!(matches!(
            file_error,
            ScanError::Policy(PolicyError::LimitExceeded {
                resource: PolicyLimitResource::FileBytes,
                observed_at_least,
                limit,
            }) if observed_at_least == encoded.len() as u64
                && limit == (encoded.len() - 1) as u64
        ));

        let line_error = scanner_with_options(
            &project,
            SearchIndexOptions {
                ignore_limits: crate::SearchIgnoreV1Limits {
                    max_line_bytes: line_bytes - 1,
                    ..exact_options.ignore_limits
                },
                ..exact_options
            },
            ScanReadLimits::default(),
        )
        .plan(ScanIntent::Full, &[], &mut AssetLoadBudget::default())
        .unwrap_err();
        assert!(matches!(
            line_error,
            ScanError::Policy(PolicyError::LimitExceeded {
                resource: PolicyLimitResource::LineBytes,
                observed_at_least,
                limit,
            }) if observed_at_least == line_bytes as u64
                && limit == (line_bytes - 1) as u64
        ));
    }

    #[test]
    fn search_ignore_rule_limit_accepts_exact_and_rejects_one_over() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        let encoded = b"exact:Assets/A.asset\nexact:Assets/B.asset\n";
        fs::write(project.path().join(SEARCH_IGNORE_V1_FILE), encoded).unwrap();
        let exact_options = SearchIndexOptions {
            ignore_limits: crate::SearchIgnoreV1Limits {
                max_file_bytes: encoded.len() as u64,
                max_line_bytes: "exact:Assets/A.asset".len(),
                max_rules: 2,
                max_parser_work: (encoded.len() as u64) * 2,
                ..crate::SearchIgnoreV1Limits::default()
            },
            ..SearchIndexOptions::default()
        };
        let exact_scanner =
            scanner_with_options(&project, exact_options, ScanReadLimits::default());

        assert!(
            exact_scanner
                .plan(ScanIntent::Full, &[], &mut AssetLoadBudget::default(),)
                .is_ok()
        );

        let error = scanner_with_options(
            &project,
            SearchIndexOptions {
                ignore_limits: crate::SearchIgnoreV1Limits {
                    max_rules: 1,
                    ..exact_options.ignore_limits
                },
                ..exact_options
            },
            ScanReadLimits::default(),
        )
        .plan(ScanIntent::Full, &[], &mut AssetLoadBudget::default())
        .unwrap_err();
        assert!(matches!(
            error,
            ScanError::Policy(PolicyError::LimitExceeded {
                resource: PolicyLimitResource::Rules,
                observed_at_least: 2,
                limit: 1,
            })
        ));
    }

    #[test]
    fn search_ignore_match_work_accepts_exact_and_rejects_one_over_without_a_plan() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        for name in ["First.asset", "Second.asset"] {
            fs::write(project.path().join("Assets").join(name), b"asset").unwrap();
        }
        fs::write(
            project.path().join(SEARCH_IGNORE_V1_FILE),
            b"glob:Assets/**/*.asset\n",
        )
        .unwrap();
        let exact_options = SearchIndexOptions {
            scan_limits: crate::ScanTraversalLimits {
                max_policy_matches: 2,
                ..crate::ScanTraversalLimits::default()
            },
            ..SearchIndexOptions::default()
        };

        let exact = scanner_with_options(&project, exact_options, ScanReadLimits::default())
            .plan(ScanIntent::Full, &[], &mut AssetLoadBudget::default())
            .unwrap();
        assert!(exact.present.is_empty());

        let error = scanner_with_options(
            &project,
            SearchIndexOptions {
                scan_limits: crate::ScanTraversalLimits {
                    max_policy_matches: 1,
                    ..exact_options.scan_limits
                },
                ..exact_options
            },
            ScanReadLimits::default(),
        )
        .plan(ScanIntent::Full, &[], &mut AssetLoadBudget::default())
        .unwrap_err();
        assert!(matches!(
            error,
            ScanError::Policy(PolicyError::LimitExceeded {
                resource: PolicyLimitResource::MatchWork,
                observed_at_least: 2,
                limit: 1,
            })
        ));
    }

    #[test]
    fn oversized_search_ignore_file_fails_before_walking_assets() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::write(project.path().join("Assets/Visible.asset"), b"asset").unwrap();
        fs::write(project.path().join(SEARCH_IGNORE_V1_FILE), vec![b'a'; 1025]).unwrap();
        let scanner = scanner_with_options(
            &project,
            SearchIndexOptions {
                ignore_limits: crate::SearchIgnoreV1Limits {
                    max_file_bytes: 1024,
                    max_line_bytes: 1024,
                    max_parser_work: 2048,
                    ..crate::SearchIgnoreV1Limits::default()
                },
                ..SearchIndexOptions::default()
            },
            ScanReadLimits::default(),
        );
        let mut budget = AssetLoadBudget::default();

        let error = scanner
            .plan(ScanIntent::Full, &[], &mut budget)
            .unwrap_err();

        assert!(matches!(
            error,
            ScanError::Policy(PolicyError::LimitExceeded {
                resource: PolicyLimitResource::FileBytes,
                observed_at_least: 1025,
                limit: 1024,
            })
        ));
        assert_eq!(budget.usage().entries, 0);
    }

    #[test]
    fn changed_root_ignore_policy_converges_to_the_same_result_as_full_scan() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        for path in ["Assets/Ignored.asset", "Assets/Keep.asset"] {
            fs::write(project.path().join(path), b"asset").unwrap();
        }
        let ignore_path = project.path().join(".unity-asset-search-ignore");
        fs::write(&ignore_path, b"exact:Assets/Ignored.asset\n").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let before = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
        let known = before
            .present
            .iter()
            .map(|candidate| candidate.relative_path().to_owned())
            .collect::<Vec<_>>();
        fs::write(&ignore_path, b"exact:Assets/Keep.asset\n").unwrap();

        let changed =
            plan_with_default_budget(&scanner, changed_paths(&scanner, [ignore_path]), &known);
        let full = plan_with_default_budget(&scanner, ScanIntent::Full, &known);

        assert_eq!(
            changed
                .present
                .iter()
                .map(ScanCandidate::relative_path)
                .collect::<Vec<_>>(),
            full.present
                .iter()
                .map(ScanCandidate::relative_path)
                .collect::<Vec<_>>()
        );
        assert_eq!(changed.deleted, full.deleted);
        assert_eq!(changed.present[0].relative_path(), "Assets/Ignored.asset");
        assert_eq!(deleted_paths(&changed), ["Assets/Keep.asset"]);
    }

    #[test]
    fn changed_root_ignore_policy_converges_when_project_root_is_a_scan_root() {
        let project = tempfile::tempdir().unwrap();
        let asset_path = project.path().join("Root.asset");
        let ignore_path = project.path().join(".unity-asset-search-ignore");
        fs::write(&asset_path, b"asset").unwrap();
        fs::write(&ignore_path, b"").unwrap();
        let paths = IndexPaths::for_project(
            project.path().to_path_buf(),
            None,
            Some(vec![project.path().to_path_buf()]),
        )
        .unwrap();
        let scanner = ProjectScanner::new(
            &paths,
            SearchIndexOptions::default(),
            ScanReadLimits::default(),
        )
        .unwrap();
        let before = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
        let known = before
            .present
            .iter()
            .map(|candidate| candidate.relative_path().to_owned())
            .collect::<Vec<_>>();
        fs::write(&ignore_path, b"exact:Root.asset\n").unwrap();

        let changed =
            plan_with_default_budget(&scanner, changed_paths(&scanner, [ignore_path]), &known);
        let full = plan_with_default_budget(&scanner, ScanIntent::Full, &known);

        assert_eq!(changed.present, full.present);
        assert_eq!(changed.deleted, full.deleted);
        assert!(changed.present.is_empty());
        assert_eq!(deleted_paths(&changed), ["Root.asset"]);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn changed_root_ignore_policy_with_case_variant_converges_to_a_full_scan() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        let asset_path = project.path().join("Assets/Hidden.asset");
        let ignore_path = project.path().join(".UNITY-ASSET-SEARCH-IGNORE");
        fs::write(&asset_path, b"asset").unwrap();
        fs::write(&ignore_path, b"").unwrap();
        #[cfg(target_os = "macos")]
        if !project.path().join(SEARCH_IGNORE_V1_FILE).exists() {
            return;
        }
        let scanner = scanner(&project, ScanReadLimits::default());
        let before = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
        let known = before
            .present
            .iter()
            .map(|candidate| candidate.relative_path().to_owned())
            .collect::<Vec<_>>();
        fs::write(&ignore_path, b"exact:Assets/Hidden.asset\n").unwrap();

        let changed =
            plan_with_default_budget(&scanner, changed_paths(&scanner, [ignore_path]), &known);
        let full = plan_with_default_budget(&scanner, ScanIntent::Full, &known);

        assert_eq!(changed.present, full.present);
        assert_eq!(changed.deleted, full.deleted);
        assert!(changed.present.is_empty());
        assert_eq!(deleted_paths(&changed), ["Assets/Hidden.asset"]);
    }

    #[test]
    fn plan_rejects_known_deletion_before_retaining_another_collection_node() {
        let project = tempfile::tempdir().unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = scanner
            .plan(
                changed_paths(&scanner, [PathBuf::from("Assets/Missing.asset")]),
                &[],
                &mut budget,
            )
            .unwrap_err();

        assert!(matches!(
            error,
            ScanError::Budget(BudgetError::Exceeded {
                resource: "entries",
                limit: 1,
                requested: 2,
            })
        ));
        assert_eq!(budget.usage().entries, 1);
    }

    #[test]
    fn plan_rejects_path_backing_before_charging_tiny_byte_budget() {
        let project = tempfile::tempdir().unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        fs::write(project.path().join("Assets/One.asset"), b"one").unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = scanner
            .plan(ScanIntent::Full, &[], &mut budget)
            .unwrap_err();

        assert!(matches!(
            error,
            ScanError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 1,
                ..
            })
        ));
        assert_eq!(budget.usage().bytes, 0);
        assert_eq!(budget.usage().entries, 0);
    }

    #[test]
    fn cloned_deletion_path_is_charged_with_its_collection_slot() {
        let project = tempfile::tempdir().unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let known = [project_source_path(&scanner, "Assets/Removed.asset")];
        let retained_path = prepared_project_source_path_clone(
            &known[0],
            &AssetLoadBudget::default(),
            "deletion budget probe",
        )
        .unwrap();
        let path_bytes = project_source_path_backing_bytes(&retained_path).unwrap();
        let mut vector_probe = Vec::<ProjectSourcePath>::new();
        vector_probe.try_reserve_exact(1).unwrap();
        let vector_bytes = checked_vec_bytes::<ProjectSourcePath>(vector_probe.capacity()).unwrap();
        let retained_bytes = path_bytes.checked_add(vector_bytes).unwrap();
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_bytes: retained_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let mut deleted = Vec::new();

        append_missing_known_paths(&known, &[], &mut deleted, &mut exact).unwrap();

        assert_eq!(deleted.len(), 1);
        assert_eq!(exact.usage().entries, 1);
        assert_eq!(exact.usage().bytes, retained_bytes);

        let mut one_byte_short = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_bytes: retained_bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = append_missing_known_paths(&known, &[], &mut Vec::new(), &mut one_byte_short)
            .unwrap_err();

        assert!(matches!(
            error,
            ScanError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == retained_bytes - 1 && requested == retained_bytes
        ));
        assert_eq!(one_byte_short.usage().entries, 0);
        assert_eq!(one_byte_short.usage().bytes, 0);
    }

    #[test]
    fn changed_path_diagnostics_have_stable_structural_order() {
        let project = tempfile::tempdir().unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let first = project.path().join("Assets/.a.asset");
        let second = project.path().join("Assets/.B.asset");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();

        let left = plan_with_default_budget(
            &scanner,
            changed_paths(&scanner, [second.clone(), first.clone()]),
            &[],
        );
        let right_intent = changed_paths(&scanner, [first.clone(), second.clone()]);
        let right = plan_with_default_budget(&scanner, right_intent, &[]);

        assert_eq!(left.diagnostics, right.diagnostics);
        assert_eq!(left.deleted, right.deleted);
        assert_eq!(left.diagnostics.len(), 2);
        assert!(matches!(
            &left.diagnostics[0],
            ScanDiagnostic::PathRejected {
                path,
                reason: PathRejection::UnsupportedFileType,
            } if path.file_name() == second.file_name()
        ));
        assert!(matches!(
            &left.diagnostics[1],
            ScanDiagnostic::PathRejected {
                path,
                reason: PathRejection::UnsupportedFileType,
            } if path.file_name() == first.file_name()
        ));
        assert!(
            left.diagnostics
                .windows(2)
                .all(|pair| { compare_scan_diagnostics(&pair[0], &pair[1]) != Ordering::Greater })
        );
    }

    #[test]
    fn shrinking_scan_roots_deletes_keys_from_the_previous_configuration() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::create_dir_all(project.path().join("Packages")).unwrap();
        fs::write(project.path().join("Assets/Current.asset"), b"current").unwrap();
        fs::write(project.path().join("Packages/Old.asset"), b"old").unwrap();
        let paths = IndexPaths::for_project(
            project.path().to_path_buf(),
            None,
            Some(vec![project.path().join("Assets")]),
        )
        .unwrap();
        let scanner = ProjectScanner::new(
            &paths,
            SearchIndexOptions::default(),
            ScanReadLimits::default(),
        )
        .unwrap();

        let plan = plan_with_default_budget(
            &scanner,
            ScanIntent::Reconcile,
            &["Packages/Old.asset".to_owned()],
        );

        assert_eq!(deleted_paths(&plan), ["Packages/Old.asset"]);
        assert_eq!(plan.present[0].relative_path(), "Assets/Current.asset");
    }

    #[test]
    fn read_source_hashes_asset_and_meta_frames_without_mtime_identity() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        let asset_path = project.path().join("Assets/Example.asset");
        fs::write(&asset_path, b"asset").unwrap();
        fs::write(
            append_meta_extension(&asset_path),
            b"fileFormatVersion: 2\nguid: ABCDEF0123456789ABCDEF0123456789\n",
        )
        .unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        let mut budget = AssetLoadBudget::default();
        let first = scanner.read_source(&plan.present[0], None, &mut budget);
        let source = first.accepted.unwrap().source;
        let second =
            scanner.read_source(&plan.present[0], Some(source.content_identity), &mut budget);

        assert_eq!(source.bytes.as_deref(), Some(&b"asset"[..]));
        assert_eq!(source.length, 5);
        assert!(source.meta_bytes.is_some());
        assert_eq!(
            source.guid.as_deref(),
            Some("abcdef0123456789abcdef0123456789")
        );
        assert_eq!(first.metrics.opened, 4);
        assert_eq!(second.metrics.opened, 4);
        assert_eq!(second.metrics.unchanged, 1);
        assert!(second.accepted.unwrap().source.unchanged);
        assert_eq!(budget.usage().entries, 2);
    }

    #[test]
    fn read_source_rejects_without_allocating_diagnostics_after_budget_is_exhausted() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::write(project.path().join("Assets/Budgeted.asset"), b"asset").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
        let meta_path_bytes = u64::try_from(
            meta_relative_path_capacity_bound(Path::new(plan.present[0].relative_path())).unwrap(),
        )
        .unwrap();
        let max_bytes = meta_path_bytes.checked_add(5).unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let outcome = scanner.read_source(&plan.present[0], None, &mut budget);
        assert!(outcome.accepted.is_none());
        assert_eq!(budget.usage().bytes, max_bytes);
        assert!(outcome.diagnostics.is_empty());
    }

    #[test]
    fn read_source_uses_static_budget_diagnostic_when_dynamic_backing_does_not_fit() {
        let project = tempfile::tempdir().unwrap();
        let deep = "a".repeat(128);
        fs::create_dir_all(project.path().join("Assets").join(&deep)).unwrap();
        fs::write(
            project
                .path()
                .join("Assets")
                .join(&deep)
                .join("Limited.asset"),
            b"12345",
        )
        .unwrap();
        let scanner = scanner(
            &project,
            ScanReadLimits {
                max_asset_bytes: 4,
                max_retained_asset_bytes: 4,
                max_meta_bytes: 1024,
            },
        );
        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
        let mut diagnostic_probe = Vec::<ScanDiagnostic>::new();
        diagnostic_probe.try_reserve_exact(1).unwrap();
        let diagnostic_bytes =
            checked_vec_bytes::<ScanDiagnostic>(diagnostic_probe.capacity()).unwrap();
        assert!(
            u64::try_from(
                meta_relative_path_capacity_bound(Path::new(plan.present[0].relative_path()))
                    .unwrap()
            )
            .unwrap()
                > diagnostic_bytes
        );
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: diagnostic_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let outcome = scanner.read_source(&plan.present[0], None, &mut budget);

        assert!(outcome.accepted.is_none());
        assert!(matches!(
            outcome.diagnostics.as_slice(),
            [ScanDiagnostic::BudgetExceeded {
                rel_path,
                part: SourcePart::Meta,
                source: BudgetError::Exceeded {
                    resource: "bytes",
                    limit,
                    ..
                },
            }] if rel_path.is_empty() && *limit == diagnostic_bytes
        ));
        assert_eq!(budget.usage().entries, 1);
        assert_eq!(budget.usage().bytes, diagnostic_bytes);
    }

    #[test]
    fn rejected_read_diagnostic_is_charged_before_return() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        let asset_path = project.path().join("Assets/Removed.asset");
        fs::write(&asset_path, b"asset").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
        fs::remove_file(asset_path).unwrap();
        let mut budget = AssetLoadBudget::default();

        let outcome = scanner.read_source(&plan.present[0], None, &mut budget);

        assert!(outcome.accepted.is_none());
        assert!(matches!(
            outcome.diagnostics.as_slice(),
            [ScanDiagnostic::ReadFailed { .. }]
        ));
        assert_eq!(budget.usage().entries, 1);
        assert_eq!(
            budget.usage().bytes,
            diagnostic_retained_bytes(&outcome.diagnostics)
        );
    }

    #[test]
    fn read_source_does_not_allocate_diagnostic_after_entry_budget_is_exhausted() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        let asset_path = project.path().join("Assets/Removed.asset");
        fs::write(&asset_path, b"asset").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
        fs::remove_file(asset_path).unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        budget.consume_entries(1).unwrap();

        let outcome = scanner.read_source(&plan.present[0], None, &mut budget);

        assert!(outcome.accepted.is_none());
        assert!(outcome.diagnostics.is_empty());
        assert_eq!(budget.usage().entries, 1);
        assert_eq!(budget.usage().bytes, 0);
    }

    #[test]
    fn framed_identity_distinguishes_asset_and_meta_boundaries() {
        let left =
            source_identity(complete_identity(b"ab"), Some(complete_identity(b"c"))).unwrap();
        let right =
            source_identity(complete_identity(b"a"), Some(complete_identity(b"bc"))).unwrap();
        let absent = source_identity(complete_identity(b"abc"), None).unwrap();
        let empty =
            source_identity(complete_identity(b"abc"), Some(complete_identity(b""))).unwrap();

        assert_ne!(left, right);
        assert_ne!(absent, empty);
    }

    #[test]
    fn source_limit_keeps_tier_zero_identity_without_partial_bytes() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::write(project.path().join("Assets/Large.asset"), b"12345").unwrap();
        let scanner = scanner(
            &project,
            ScanReadLimits {
                max_asset_bytes: 4,
                max_retained_asset_bytes: 4,
                max_meta_bytes: 1024,
            },
        );
        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        let mut budget = AssetLoadBudget::default();
        let outcome = scanner.read_source(&plan.present[0], None, &mut budget);

        let source = &outcome.accepted.as_ref().unwrap().source;
        assert!(source.bytes.is_none());
        assert!(!source.unchanged);
        assert_eq!(outcome.metrics.opened, 2);
        assert_eq!(budget.usage().entries, 2);
        assert_eq!(
            budget.usage().bytes,
            read_source_non_payload_retained_bytes(&plan.present[0], source, &outcome.diagnostics)
        );
        assert!(matches!(
            outcome.diagnostics.as_slice(),
            [ScanDiagnostic::LimitExceeded {
                part: SourcePart::Asset,
                observed_at_least: 5,
                limit: 4,
                ..
            }]
        ));
    }

    #[test]
    fn metadata_limit_keeps_asset_identity_and_reports_incomplete_metadata() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        let asset_path = project.path().join("Assets/LargeMeta.asset");
        fs::write(&asset_path, b"asset").unwrap();
        fs::write(append_meta_extension(&asset_path), b"guid: 0123456789").unwrap();
        let scanner = scanner(
            &project,
            ScanReadLimits {
                max_asset_bytes: 1024,
                max_retained_asset_bytes: 1024,
                max_meta_bytes: 4,
            },
        );
        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        let outcome = scanner.read_source(&plan.present[0], None, &mut AssetLoadBudget::default());
        let source = &outcome.accepted.as_ref().unwrap().source;

        assert_eq!(source.bytes.as_deref(), Some(&b"asset"[..]));
        assert!(source.meta_bytes.is_none());
        assert!(source.guid.is_none());
        assert!(!source.unchanged);
        assert!(matches!(
            outcome.diagnostics.as_slice(),
            [ScanDiagnostic::LimitExceeded {
                part: SourcePart::Meta,
                observed_at_least: 16,
                limit: 4,
                ..
            }]
        ));
    }

    #[test]
    fn large_source_is_hashed_once_without_retaining_its_payload() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::write(project.path().join("Assets/Large.bin"), b"12345").unwrap();
        let scanner = scanner(
            &project,
            ScanReadLimits {
                max_asset_bytes: 1024,
                max_retained_asset_bytes: 4,
                max_meta_bytes: 1024,
            },
        );
        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        let outcome = scanner.read_source(&plan.present[0], None, &mut AssetLoadBudget::default());
        let source = outcome.accepted.unwrap().source;

        assert!(source.bytes.is_none());
        assert_eq!(source.length, 5);
        assert_eq!(outcome.metrics.opened, 2);
        assert_eq!(outcome.metrics.read_bytes, 5);
        assert!(matches!(
            outcome.diagnostics.as_slice(),
            [ScanDiagnostic::PayloadNotRetained {
                length: 5,
                retained_limit: 4,
                ..
            }]
        ));
    }

    #[test]
    fn source_replacement_after_read_is_rejected_by_handle_identity() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        let asset_path = project.path().join("Assets/Replaced.asset");
        fs::write(&asset_path, b"before").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let relative = Path::new("Assets/Replaced.asset");
        let file = scanner.open_read_file(relative, SourcePart::Asset).unwrap();
        let mut metrics = ScanMetrics::default();
        let mut budget = AssetLoadBudget::default();
        let blob = read_file_once(
            file,
            SourcePart::Asset,
            1024,
            1024,
            &mut metrics,
            &mut budget,
        )
        .unwrap();
        fs::remove_file(&asset_path).unwrap();
        fs::write(&asset_path, b"after!").unwrap();

        let error = scanner
            .revalidate_read_blob(relative, SourcePart::Asset, &blob, &mut metrics)
            .unwrap_err();

        assert!(matches!(error, ReadFailure::Changed { .. }));
    }

    #[test]
    fn meta_guid_must_be_unique_and_at_document_root() {
        let valid = b"fileFormatVersion: 2\n  guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nguid: BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\n";
        assert_eq!(
            guid_value_from_meta(valid),
            (Some(&b"BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"[..]), false)
        );
        assert_eq!(
            guid_value_from_meta(b"nested:\n  guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"),
            (None, true)
        );
        assert_eq!(
            guid_value_from_meta(
                b"guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nguid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n"
            ),
            (None, true)
        );
    }

    #[test]
    fn changed_directory_rescan_reports_deleted_descendants() {
        let project = tempfile::tempdir().unwrap();
        let directory = project.path().join("Assets/Area");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("Present.asset"), b"present").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let known = [
            "Assets/Area/Present.asset".to_owned(),
            "Assets/Area/Removed.asset".to_owned(),
        ];

        let intent = changed_paths(&scanner, [directory]);
        let plan = plan_with_default_budget(&scanner, intent, &known);
        assert_eq!(deleted_paths(&plan), ["Assets/Area/Removed.asset"]);
        assert_eq!(
            plan.present
                .iter()
                .map(ScanCandidate::relative_path)
                .collect::<Vec<_>>(),
            ["Assets/Area/Present.asset"]
        );
    }

    #[test]
    fn deleted_directory_keeps_prefix_and_expands_known_descendants() {
        let project = tempfile::tempdir().unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let known = [
            "Assets/Removed/A.asset".to_owned(),
            "Assets/Removed/Nested/B.asset".to_owned(),
        ];

        let plan = plan_with_default_budget(
            &scanner,
            changed_paths(&scanner, [PathBuf::from("Assets/Removed")]),
            &known,
        );

        let mut deleted = deleted_paths(&plan);
        deleted.sort_unstable();
        assert_eq!(
            deleted,
            [
                "Assets/Removed",
                "Assets/Removed/A.asset",
                "Assets/Removed/Nested/B.asset"
            ]
        );
        assert!(plan.present.is_empty());
    }

    #[test]
    fn changed_path_absence_proofs_reject_recreated_files_and_directories() {
        for relative in ["Assets/Removed.asset", "Assets/Removed"] {
            let project = tempfile::tempdir().unwrap();
            let scanner = scanner(&project, ScanReadLimits::default());
            let known = if relative.ends_with(".asset") {
                vec![relative.to_owned()]
            } else {
                vec![format!("{relative}/Nested.asset")]
            };
            let known = project_source_paths(&scanner, &known);
            let mut budget = AssetLoadBudget::default();
            let plan = scanner
                .plan(
                    changed_paths(&scanner, [PathBuf::from(relative)]),
                    &known,
                    &mut budget,
                )
                .unwrap();

            if relative.ends_with(".asset") {
                fs::write(project.path().join(relative), b"recreated").unwrap();
            } else {
                fs::create_dir(project.path().join(relative)).unwrap();
            }

            assert!(matches!(
                scanner.validate_plan(&plan),
                Err(ScanError::SourceChangedDuringScan)
            ));
        }
    }

    #[test]
    fn custom_index_root_inside_scan_root_is_excluded() {
        let project = crate::secure_test_tempdir();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::write(project.path().join("Assets/Public.asset"), b"public").unwrap();
        let index_root = project.path().join("Assets/SearchIndex");
        let paths =
            IndexPaths::for_project(project.path().to_path_buf(), Some(index_root.clone()), None)
                .unwrap();
        fs::write(paths.index_root().join("Internal.asset"), b"internal").unwrap();
        let scanner = ProjectScanner::new(
            &paths,
            SearchIndexOptions::default(),
            ScanReadLimits::default(),
        )
        .unwrap();

        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        assert_eq!(
            plan.present
                .iter()
                .map(ScanCandidate::relative_path)
                .collect::<Vec<_>>(),
            ["Assets/Public.asset"]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_case_alias_of_an_index_namespace_is_excluded_from_scanning() {
        let temporary = crate::secure_test_tempdir();
        let project = temporary.path().join("Project");
        fs::create_dir_all(project.join("Assets")).unwrap();
        fs::write(project.join("Assets").join("Public.asset"), b"public").unwrap();
        let project_alias = temporary.path().join("project");
        if !project_alias.exists() {
            return;
        }
        let namespace_alias = project_alias.join("assets").join("searchindex");
        let paths = IndexPaths::for_project(project_alias, Some(namespace_alias), None).unwrap();
        fs::write(paths.index_root().join("Internal.asset"), b"internal").unwrap();
        let scanner = ProjectScanner::new(
            &paths,
            SearchIndexOptions::default(),
            ScanReadLimits::default(),
        )
        .unwrap();

        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        assert_eq!(
            plan.present
                .iter()
                .map(ScanCandidate::relative_path)
                .collect::<Vec<_>>(),
            ["Assets/Public.asset"]
        );
    }

    #[test]
    fn shared_index_namespace_inside_scan_root_excludes_every_project_child() {
        let temporary = crate::secure_test_tempdir();
        let project = temporary.path().join("project");
        let other_project = temporary.path().join("other-project");
        fs::create_dir_all(project.join("Assets")).unwrap();
        fs::create_dir_all(other_project.join("Assets")).unwrap();
        fs::write(project.join("Assets/Public.asset"), b"public").unwrap();
        let index_namespace = project.join("Assets/SearchIndex");
        let other_paths =
            IndexPaths::for_project(other_project, Some(index_namespace.clone()), None).unwrap();
        fs::write(
            other_paths.index_root().join("Foreign.asset"),
            b"foreign private state",
        )
        .unwrap();
        let paths = IndexPaths::for_project(project, Some(index_namespace.clone()), None).unwrap();
        assert_eq!(paths.index_namespace_root(), index_namespace);
        let scanner = ProjectScanner::new(
            &paths,
            SearchIndexOptions::default(),
            ScanReadLimits::default(),
        )
        .unwrap();

        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        assert_eq!(
            plan.present
                .iter()
                .map(ScanCandidate::relative_path)
                .collect::<Vec<_>>(),
            ["Assets/Public.asset"]
        );
    }

    #[test]
    fn project_root_scan_excludes_unity_state_and_nested_index_subtrees() {
        let project = crate::secure_test_tempdir();
        let index_root = project.path().join("SearchIndex");
        for relative in [
            ".git/Hidden.asset",
            "Library/Hidden.asset",
            "Temp/Hidden.asset",
            "Obj/Hidden.asset",
            "Logs/Hidden.asset",
            "Visible/Keep.asset",
        ] {
            let path = project.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, relative.as_bytes()).unwrap();
        }
        let paths = IndexPaths::for_project(
            project.path().to_path_buf(),
            Some(index_root.clone()),
            Some(vec![project.path().to_path_buf()]),
        )
        .unwrap();
        fs::write(paths.index_root().join("Internal.asset"), b"internal").unwrap();
        let scanner = ProjectScanner::new(
            &paths,
            SearchIndexOptions::default(),
            ScanReadLimits::default(),
        )
        .unwrap();

        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        assert_eq!(
            plan.present
                .iter()
                .map(ScanCandidate::relative_path)
                .collect::<Vec<_>>(),
            ["Visible/Keep.asset"]
        );
        assert!(plan.diagnostics.is_empty());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn symlink_escape_is_rejected_from_changed_paths() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("Outside.asset");
        fs::write(&outside_file, b"outside").unwrap();
        let link = project.path().join("Assets/Escape.asset");
        create_file_symlink(&outside_file, &link).unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());

        let intent = changed_paths(&scanner, [link]);
        let plan = plan_with_default_budget(&scanner, intent, &[]);

        assert!(plan.present.is_empty());
        assert!(plan.diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic,
                ScanDiagnostic::PathRejected {
                    reason: PathRejection::Symlink,
                    ..
                }
            )
        }));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn directory_symlink_cycle_is_diagnosed_without_traversal() {
        let project = tempfile::tempdir().unwrap();
        let nested = project.path().join("Assets").join("Nested");
        fs::create_dir_all(&nested).unwrap();
        let cycle = nested.join("Cycle");
        create_directory_symlink(project.path().join("Assets"), &cycle).unwrap();

        let plan = plan_with_default_budget(
            &scanner(&project, ScanReadLimits::default()),
            ScanIntent::Full,
            &[],
        );

        assert!(plan.present.is_empty());
        assert!(
            plan.diagnostics.iter().any(|diagnostic| {
                matches!(
                    diagnostic,
                    ScanDiagnostic::PathRejected {
                        path,
                        reason: PathRejection::Symlink,
                    } if path.ends_with("Assets/Nested/Cycle")
                )
            }),
            "unexpected diagnostics: {:#?}",
            plan.diagnostics
        );
    }

    #[cfg(windows)]
    #[test]
    fn directory_junction_cycle_is_diagnosed_without_traversal() {
        let project = tempfile::tempdir().unwrap();
        let nested = project.path().join("Assets").join("Nested");
        fs::create_dir_all(&nested).unwrap();
        let cycle = nested.join("Cycle");
        create_directory_junction(&cycle, &project.path().join("Assets"));

        let plan = plan_with_default_budget(
            &scanner(&project, ScanReadLimits::default()),
            ScanIntent::Full,
            &[],
        );
        fs::remove_dir(&cycle).unwrap();

        assert!(plan.present.is_empty());
        assert!(
            plan.diagnostics.iter().any(|diagnostic| {
                matches!(
                    diagnostic,
                    ScanDiagnostic::PathRejected {
                        path,
                        reason: PathRejection::Symlink,
                    } if path.ends_with("Assets/Nested/Cycle")
                )
            }),
            "unexpected diagnostics: {:#?}",
            plan.diagnostics
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn create_file_symlink(target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn create_directory_symlink(target: PathBuf, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_directory_junction(link: &Path, target: &Path) {
        use std::process::Command;

        let output = Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to create junction: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
