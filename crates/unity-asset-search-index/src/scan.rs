mod ignore_policy;
mod platform;

use std::cmp::Ordering;
use std::collections::TryReserveError;
use std::error::Error as StdError;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, Metadata};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use ignore::{DirEntry, WalkBuilder};
use same_file::Handle;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedSourceBytes, DigestBuildError, DigestV1, DigestV1Builder,
    string_allocation_bytes, vec_allocation_bytes,
};
use unity_asset_search_core::SearchKind;

use self::ignore_policy::{
    IgnoreLimitResource, IgnoreReadOperation, IgnoreSyntaxReason, RootIgnoreMatcher,
    is_configured_project_root_ignore_file, is_named_project_root_ignore_file,
};
use self::platform::ProjectReadRoot;
use crate::{IndexPaths, SearchIndexOptions};

const SOURCE_IDENTITY_DOMAIN: &[u8] = b"unity-asset-search:source:v3";
const ASSET_COMPLETE: &[u8] = b"asset:complete";
const ASSET_UNAVAILABLE: &[u8] = b"asset:unavailable";
const META_PRESENT: &[u8] = b"meta:present";
const META_UNAVAILABLE: &[u8] = b"meta:unavailable";
const META_ABSENT: &[u8] = b"meta:absent";
const READ_BUFFER_BYTES: usize = 64 * 1024;
const READ_META_PATH_EXTRA_CAPACITY: usize = 16;
#[cfg(not(windows))]
const READ_CANONICAL_PATH_EXTRA_CAPACITY: usize = 0;
#[cfg(windows)]
const READ_CANONICAL_PATH_EXTRA_CAPACITY: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScanIntent {
    Full,
    Reconcile,
    ChangedPaths(Vec<PathBuf>),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanCandidate {
    pub(crate) rel_path: String,
    pub(crate) abs_path: PathBuf,
    pub(crate) name: String,
    pub(crate) kind: SearchKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanPlan {
    pub(crate) mode: ScanMode,
    pub(crate) changed: Vec<String>,
    pub(crate) present: Vec<ScanCandidate>,
    pub(crate) deleted: Vec<String>,
    /// An empty prefix denotes the complete project root.
    pub(crate) rescan_prefixes: Vec<String>,
    pub(crate) diagnostics: Vec<ScanDiagnostic>,
    pub(crate) metrics: ScanMetrics,
}

#[derive(Debug)]
pub(crate) enum ScanError {
    Budget(BudgetError),
    AllocationFailed {
        allocation: &'static str,
        requested: usize,
        source: TryReserveError,
    },
    IgnoreIo {
        file: &'static str,
        operation: IgnoreReadOperation,
        source: io::Error,
    },
    IgnoreLimitExceeded {
        file: &'static str,
        resource: IgnoreLimitResource,
        observed_at_least: u64,
        limit: u64,
    },
    IgnoreChangedDuringRead {
        file: &'static str,
    },
    IgnoreSyntax {
        file: &'static str,
        line: Option<u64>,
        reason: IgnoreSyntaxReason,
    },
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
            Self::IgnoreIo {
                file,
                operation,
                source,
            } => write!(
                formatter,
                "failed to {operation} project-root ignore file {file}: {source}"
            ),
            Self::IgnoreLimitExceeded {
                file,
                resource,
                observed_at_least,
                limit,
            } => write!(
                formatter,
                "project-root ignore {resource} limit exceeded for {file}: observed at least \
                 {observed_at_least}, limit {limit}"
            ),
            Self::IgnoreChangedDuringRead { file } => {
                write!(
                    formatter,
                    "project-root ignore file changed while reading: {file}"
                )
            }
            Self::IgnoreSyntax {
                file,
                line: Some(line),
                reason,
            } => write!(
                formatter,
                "invalid project-root ignore rule in {file}:{line}: {reason}"
            ),
            Self::IgnoreSyntax {
                file,
                line: None,
                reason,
            } => write!(
                formatter,
                "invalid compiled project-root ignore policy from {file}: {reason}"
            ),
        }
    }
}

impl StdError for ScanError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Budget(error) => Some(error),
            Self::AllocationFailed { source, .. } => Some(source),
            Self::IgnoreIo { source, .. } => Some(source),
            Self::IgnoreLimitExceeded { .. }
            | Self::IgnoreChangedDuringRead { .. }
            | Self::IgnoreSyntax { .. } => None,
        }
    }
}

impl From<BudgetError> for ScanError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SourcePart {
    Asset,
    Meta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PathRejection {
    InvalidPath,
    OutsideProject,
    OutsideScanRoots,
    InsideIndexRoot,
    Excluded,
    Symlink,
    UnsupportedFileType,
    NonUtf8RelativePath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScanDiagnostic {
    WalkFailed {
        message: String,
    },
    PathRejected {
        path: PathBuf,
        reason: PathRejection,
    },
    ReadFailed {
        rel_path: String,
        part: SourcePart,
        kind: io::ErrorKind,
        message: String,
    },
    LimitExceeded {
        rel_path: String,
        part: SourcePart,
        observed_at_least: u64,
        limit: u64,
    },
    AllocationFailed {
        rel_path: String,
        part: SourcePart,
        requested: u64,
    },
    BudgetExceeded {
        rel_path: String,
        part: SourcePart,
        source: BudgetError,
    },
    ChangedDuringRead {
        rel_path: String,
        part: SourcePart,
        before: Option<FileHint>,
        after: Option<FileHint>,
    },
    DigestFailed {
        rel_path: String,
        message: String,
    },
    MalformedGuid {
        rel_path: String,
    },
    PayloadNotRetained {
        rel_path: String,
        length: u64,
        retained_limit: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FileHint {
    pub(crate) size: u64,
    pub(crate) mtime_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceHints {
    pub(crate) asset: FileHint,
    pub(crate) meta: Option<FileHint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadSource {
    pub(crate) rel_path: String,
    pub(crate) abs_path: PathBuf,
    pub(crate) name: String,
    pub(crate) kind: SearchKind,
    pub(crate) guid: Option<String>,
    pub(crate) bytes: Option<BudgetedSourceBytes>,
    pub(crate) meta_bytes: Option<BudgetedSourceBytes>,
    pub(crate) length: u64,
    pub(crate) content_identity: DigestV1,
    pub(crate) hints: SourceHints,
    pub(crate) unchanged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadSourceOutcome {
    pub(crate) source: Option<ReadSource>,
    pub(crate) diagnostics: Vec<ScanDiagnostic>,
    pub(crate) metrics: ScanMetrics,
}

impl ReadSourceOutcome {
    fn rejected_without_diagnostic(metrics: ScanMetrics) -> Self {
        Self {
            source: None,
            diagnostics: Vec::new(),
            metrics,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectScanner {
    project_root: PathBuf,
    read_root: ProjectReadRoot,
    scan_roots: Vec<PathBuf>,
    index_root: PathBuf,
    options: SearchIndexOptions,
    limits: ScanReadLimits,
}

impl ProjectScanner {
    pub(crate) fn new(
        paths: &IndexPaths,
        options: SearchIndexOptions,
        limits: ScanReadLimits,
    ) -> Result<Self> {
        let project_root = canonical_directory(paths.project_root()).with_context(|| {
            format!(
                "canonicalize project root: {}",
                paths.project_root().display()
            )
        })?;
        let read_root = ProjectReadRoot::open(&project_root).with_context(|| {
            format!(
                "open identity-bound project root: {}",
                project_root.display()
            )
        })?;
        let mut scan_roots = paths
            .scan_roots()
            .iter()
            .map(|root| {
                canonical_directory(root)
                    .with_context(|| format!("canonicalize scan root: {}", root.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        if let Some(root) = scan_roots
            .iter()
            .find(|root| !root.starts_with(&project_root))
        {
            return Err(anyhow!(
                "scan root must remain inside project root: {}",
                root.display()
            ));
        }
        scan_roots.sort_unstable();
        scan_roots.dedup();
        if scan_roots.is_empty() {
            return Err(anyhow!("at least one scan root is required"));
        }

        let index_root = resolve_allow_missing(&absolute_from(&project_root, paths.index_root())?)
            .with_context(|| {
                format!(
                    "resolve index root boundary: {}",
                    paths.index_root().display()
                )
            })?;

        Ok(Self {
            project_root,
            read_root,
            scan_roots,
            index_root,
            options,
            limits,
        })
    }

    pub(crate) fn plan(
        &self,
        intent: ScanIntent,
        known_rel_paths: &[String],
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<ScanPlan, ScanError> {
        let mut diagnostics = Vec::new();
        self.diagnose_known_paths(known_rel_paths, &mut diagnostics, budget)?;
        let (mode, mut present, mut deleted, mut rescan_prefixes) = match intent {
            ScanIntent::Full => {
                let mut present = self.discover_full(&mut diagnostics, budget)?;
                sort_and_dedup_candidates(&mut present);
                let mut deleted = Vec::new();
                append_missing_known_paths(known_rel_paths, &present, &mut deleted, budget)?;
                (ScanMode::Full, present, deleted, Vec::new())
            }
            ScanIntent::Reconcile => {
                let mut present = self.discover_full(&mut diagnostics, budget)?;
                sort_and_dedup_candidates(&mut present);
                let mut deleted = Vec::new();
                append_missing_known_paths(known_rel_paths, &present, &mut deleted, budget)?;
                (ScanMode::Reconcile, present, deleted, Vec::new())
            }
            ScanIntent::ChangedPaths(paths) => {
                let changed =
                    self.discover_changed(paths, known_rel_paths, &mut diagnostics, budget)?;
                (
                    ScanMode::ChangedPaths,
                    changed.present,
                    changed.deleted,
                    changed.rescan_prefixes,
                )
            }
        };
        sort_and_dedup_candidates(&mut present);
        diagnostics.sort_unstable_by(compare_scan_diagnostics);
        diagnostics.dedup();

        deleted.sort_unstable();
        deleted.dedup();

        let changed = merge_changed_paths(&present, &deleted, budget)?;

        rescan_prefixes.sort_unstable();
        rescan_prefixes.dedup();

        let metrics = ScanMetrics {
            discovered: saturating_usize_to_u64(present.len()),
            deleted: saturating_usize_to_u64(deleted.len()),
            ..ScanMetrics::default()
        };

        Ok(ScanPlan {
            mode,
            changed,
            present,
            deleted,
            rescan_prefixes,
            diagnostics,
            metrics,
        })
    }

    pub(crate) fn read_source(
        &self,
        candidate: &ScanCandidate,
        previous_identity: Option<DigestV1>,
        budget: &mut AssetLoadBudget,
    ) -> ReadSourceOutcome {
        let mut metrics = ScanMetrics::default();
        if !is_supported_asset_path(&candidate.abs_path) {
            return rejected_with_spec(
                ReadDiagnosticSpec::PathRejected {
                    path: DiagnosticPath::Borrowed(&candidate.abs_path),
                    reason: PathRejection::UnsupportedFileType,
                    part: SourcePart::Asset,
                },
                metrics,
                budget,
            );
        }
        let metadata = match fs::symlink_metadata(&candidate.abs_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                return rejected_with_spec(
                    ReadDiagnosticSpec::ReadFailed {
                        rel_path: &candidate.rel_path,
                        part: SourcePart::Asset,
                        kind: error.kind(),
                        message: DiagnosticMessage::Io(&error),
                    },
                    metrics,
                    budget,
                );
            }
        };
        if metadata.file_type().is_symlink() {
            return rejected_with_spec(
                ReadDiagnosticSpec::PathRejected {
                    path: DiagnosticPath::Borrowed(&candidate.abs_path),
                    reason: PathRejection::Symlink,
                    part: SourcePart::Asset,
                },
                metrics,
                budget,
            );
        }
        if !metadata.is_file() {
            return rejected_with_spec(
                ReadDiagnosticSpec::PathRejected {
                    path: DiagnosticPath::Borrowed(&candidate.abs_path),
                    reason: PathRejection::UnsupportedFileType,
                    part: SourcePart::Asset,
                },
                metrics,
                budget,
            );
        }
        let canonical_backing_bound = match read_canonical_path_backing_bound(&candidate.abs_path) {
            Ok(bound) => bound,
            Err(error) => {
                return rejected_scan_error(error, SourcePart::Asset, metrics, budget);
            }
        };
        if let Err(source) = budget.consume_bytes(canonical_backing_bound) {
            return rejected_scan_error(
                ScanError::Budget(source),
                SourcePart::Asset,
                metrics,
                budget,
            );
        }
        let canonical = match candidate.abs_path.canonicalize() {
            Ok(canonical) => canonical,
            Err(error) => {
                return rejected_with_spec(
                    ReadDiagnosticSpec::ReadFailed {
                        rel_path: &candidate.rel_path,
                        part: SourcePart::Asset,
                        kind: error.kind(),
                        message: DiagnosticMessage::Io(&error),
                    },
                    metrics,
                    budget,
                );
            }
        };
        let canonical_backing = u64::try_from(canonical.capacity()).unwrap_or(u64::MAX);
        if canonical != candidate.abs_path || canonical_backing > canonical_backing_bound {
            return rejected_with_spec(
                ReadDiagnosticSpec::PathRejected {
                    path: DiagnosticPath::Borrowed(&candidate.abs_path),
                    reason: PathRejection::InvalidPath,
                    part: SourcePart::Asset,
                },
                metrics,
                budget,
            );
        }
        if let Err(reason) = self.validate_boundary(&canonical) {
            return rejected_with_spec(
                ReadDiagnosticSpec::PathRejected {
                    path: DiagnosticPath::Borrowed(&canonical),
                    reason,
                    part: SourcePart::Asset,
                },
                metrics,
                budget,
            );
        }
        let normalized = match normalized_rel_str(&self.project_root, &canonical) {
            Ok(normalized) => normalized,
            Err(reason) => {
                return rejected_with_spec(
                    ReadDiagnosticSpec::PathRejected {
                        path: DiagnosticPath::Borrowed(&canonical),
                        reason,
                        part: SourcePart::Asset,
                    },
                    metrics,
                    budget,
                );
            }
        };
        if !normalized_rel_path_matches(normalized, &candidate.rel_path) {
            return rejected_with_spec(
                ReadDiagnosticSpec::PathRejected {
                    path: DiagnosticPath::Borrowed(&canonical),
                    reason: PathRejection::InvalidPath,
                    part: SourcePart::Asset,
                },
                metrics,
                budget,
            );
        }

        let asset_relative = Path::new(&candidate.rel_path);
        let meta_relative = match retained_meta_relative_path(asset_relative, budget) {
            Ok(path) => path,
            Err(failure) => {
                return rejected_read_failure(
                    &failure,
                    &candidate.rel_path,
                    DiagnosticPath::Borrowed(&candidate.abs_path),
                    metrics,
                    budget,
                );
            }
        };
        let asset_handle = match self.open_read_handle(asset_relative, SourcePart::Asset) {
            Ok(handle) => handle,
            Err(failure) => {
                return rejected_read_failure(
                    &failure,
                    &candidate.rel_path,
                    DiagnosticPath::Joined {
                        root: &self.project_root,
                        relative: asset_relative,
                    },
                    metrics,
                    budget,
                );
            }
        };
        let meta_handle = match self.read_root.open_relative(&meta_relative) {
            Ok(file) => match Handle::from_file(file) {
                Ok(handle) => Some(handle),
                Err(error) => {
                    let failure = ReadFailure::Io {
                        part: SourcePart::Meta,
                        context: ReadIoContext::Read,
                        source: error,
                    };
                    return rejected_read_failure(
                        &failure,
                        &candidate.rel_path,
                        DiagnosticPath::Joined {
                            root: &self.project_root,
                            relative: &meta_relative,
                        },
                        metrics,
                        budget,
                    );
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                let failure = ReadFailure::Io {
                    part: SourcePart::Meta,
                    context: ReadIoContext::Open,
                    source: error,
                };
                return rejected_read_failure(
                    &failure,
                    &candidate.rel_path,
                    DiagnosticPath::Joined {
                        root: &self.project_root,
                        relative: &meta_relative,
                    },
                    metrics,
                    budget,
                );
            }
        };

        let asset = match read_file_once(
            asset_handle,
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
                    &candidate.rel_path,
                    DiagnosticPath::Joined {
                        root: &self.project_root,
                        relative: asset_relative,
                    },
                    metrics,
                    budget,
                );
            }
        };

        let meta = match meta_handle {
            Some(handle) => match read_file_once(
                handle,
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
                        &candidate.rel_path,
                        DiagnosticPath::Joined {
                            root: &self.project_root,
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
                &candidate.rel_path,
                DiagnosticPath::Joined {
                    root: &self.project_root,
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
                    &candidate.rel_path,
                    DiagnosticPath::Joined {
                        root: &self.project_root,
                        relative: &meta_relative,
                    },
                    metrics,
                    budget,
                );
            }
        } else if let Err(failure) = self.revalidate_absent_meta(&meta_relative, &mut metrics) {
            return rejected_read_failure(
                &failure,
                &candidate.rel_path,
                DiagnosticPath::Joined {
                    root: &self.project_root,
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
                    &candidate.rel_path,
                    DiagnosticPath::Borrowed(&canonical),
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
                .map(|note| read_note_spec(note, &candidate.rel_path)),
            meta.as_ref()
                .and_then(|meta| meta.diagnostic)
                .map(|note| read_note_spec(note, &candidate.rel_path)),
            malformed_guid.then_some(ReadDiagnosticSpec::MalformedGuid {
                rel_path: &candidate.rel_path,
            }),
            (asset.digest.is_some() && asset.bytes.is_none()).then_some(
                ReadDiagnosticSpec::PayloadNotRetained {
                    rel_path: &candidate.rel_path,
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
        let source = ReadSource {
            rel_path: prepared.rel_path,
            abs_path: canonical,
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

        ReadSourceOutcome {
            source: Some(source),
            diagnostics,
            metrics,
        }
    }

    fn discover_full(
        &self,
        diagnostics: &mut Vec<ScanDiagnostic>,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<Vec<ScanCandidate>, ScanError> {
        let ignore_matcher = RootIgnoreMatcher::load(&self.read_root, self.options, budget)?;
        let mut scan_roots = Vec::new();
        for root in &self.scan_roots {
            let root = prepared_path_clone(root, budget, "full scan root clone")?;
            push_retained(
                &mut scan_roots,
                root,
                path_backing_bytes,
                budget,
                "full scan root list",
            )?;
        }
        let project_root =
            retained_path_clone(&self.project_root, budget, "full scan project root")?;
        let index_root = retained_path_clone(&self.index_root, budget, "full scan index root")?;
        let mut builder = WalkBuilder::new(&self.project_root);
        configure_walk_builder(&mut builder);
        builder.filter_entry(move |entry: &DirEntry| {
            let path = entry.path();
            if entry.file_type().is_some_and(|kind| kind.is_symlink())
                || is_excluded_under(&project_root, path)
                || path.starts_with(&index_root)
            {
                return false;
            }
            if path == project_root {
                return true;
            }
            scan_roots
                .iter()
                .any(|root| root.starts_with(path) || path.starts_with(root))
        });
        let present = self.collect_walk(builder, diagnostics, budget, ignore_matcher.as_ref())?;
        ignore_matcher.validate_current(&self.read_root, self.options)?;
        Ok(present)
    }

    fn discover_changed(
        &self,
        mut paths: Vec<PathBuf>,
        known: &[String],
        diagnostics: &mut Vec<ScanDiagnostic>,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<ChangedDiscovery, ScanError> {
        paths.sort_unstable();
        paths.dedup();
        let mut exact_files = Vec::new();
        let mut rescan_dirs = Vec::new();
        let mut requested_rel_paths = Vec::new();
        let mut deleted = Vec::new();
        let mut rescan_prefixes = Vec::new();

        for supplied in paths {
            preflight_changed_path(&self.project_root, &supplied, budget)?;
            let supplied = match absolute_from(&self.project_root, &supplied) {
                Ok(path) => path,
                Err(_) => {
                    push_diagnostic(
                        diagnostics,
                        ScanDiagnostic::PathRejected {
                            path: supplied,
                            reason: PathRejection::InvalidPath,
                        },
                        budget,
                    )?;
                    continue;
                }
            };
            if is_named_project_root_ignore_file(&supplied, self.options)
                && let Ok(resolved_policy_path) = resolve_allow_missing(&supplied)
                && is_configured_project_root_ignore_file(
                    &self.project_root,
                    &resolved_policy_path,
                    self.options,
                )
            {
                for root in &self.scan_roots {
                    let root = prepared_path_clone(root, budget, "changed scan root clone")?;
                    push_retained(
                        &mut rescan_dirs,
                        root,
                        path_backing_bytes,
                        budget,
                        "changed rescan directory list",
                    )?;
                }
                continue;
            }
            let supplied = if is_meta_path(&supplied) {
                asset_path_from_meta(&supplied).unwrap_or(supplied)
            } else {
                supplied
            };
            let resolved = match resolve_allow_missing(&supplied) {
                Ok(path) => path,
                Err(error) => {
                    push_diagnostic(
                        diagnostics,
                        ScanDiagnostic::PathRejected {
                            path: supplied,
                            reason: path_error_reason(&error),
                        },
                        budget,
                    )?;
                    continue;
                }
            };
            if let Err(reason) = self.validate_boundary(&resolved) {
                push_diagnostic(
                    diagnostics,
                    ScanDiagnostic::PathRejected {
                        path: resolved,
                        reason,
                    },
                    budget,
                )?;
                continue;
            }
            let rel_path = match prepared_normalized_rel_path(
                &self.project_root,
                &resolved,
                budget,
                "requested changed path",
            )? {
                Ok(path) => path,
                Err(reason) => {
                    push_diagnostic(
                        diagnostics,
                        ScanDiagnostic::PathRejected {
                            path: resolved,
                            reason,
                        },
                        budget,
                    )?;
                    continue;
                }
            };
            let mut requested = RequestedPath {
                rel_path,
                delete_unconditionally: false,
            };

            match fs::symlink_metadata(&supplied) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    push_diagnostic(
                        diagnostics,
                        ScanDiagnostic::PathRejected {
                            path: supplied,
                            reason: PathRejection::Symlink,
                        },
                        budget,
                    )?;
                }
                Ok(metadata) if metadata.is_file() => {
                    if is_supported_asset_path(&resolved) {
                        push_retained(
                            &mut exact_files,
                            resolved,
                            path_backing_bytes,
                            budget,
                            "changed exact file list",
                        )?;
                    }
                }
                Ok(metadata) if metadata.is_dir() => {
                    push_retained(
                        &mut rescan_dirs,
                        resolved,
                        path_backing_bytes,
                        budget,
                        "changed rescan directory list",
                    )?;
                }
                Ok(_) => {
                    push_diagnostic(
                        diagnostics,
                        ScanDiagnostic::PathRejected {
                            path: supplied,
                            reason: PathRejection::UnsupportedFileType,
                        },
                        budget,
                    )?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    requested.delete_unconditionally = true;
                    let has_known_descendants = known.iter().any(|known_path| {
                        known_path != &requested.rel_path
                            && is_path_at_or_below(known_path, &requested.rel_path)
                    });
                    if has_known_descendants {
                        let prefix = prepared_string_clone(
                            &requested.rel_path,
                            budget,
                            "missing rescan prefix",
                        )?;
                        push_retained(
                            &mut rescan_prefixes,
                            prefix,
                            string_backing_bytes,
                            budget,
                            "rescan prefix list",
                        )?;
                    }
                }
                Err(error) => {
                    let diagnostic_rel_path = prepared_string_clone(
                        &requested.rel_path,
                        budget,
                        "changed path diagnostic",
                    )?;
                    push_diagnostic(
                        diagnostics,
                        ScanDiagnostic::ReadFailed {
                            rel_path: diagnostic_rel_path,
                            part: SourcePart::Asset,
                            kind: error.kind(),
                            message: error.to_string(),
                        },
                        budget,
                    )?;
                }
            }
            push_retained(
                &mut requested_rel_paths,
                requested,
                requested_path_backing_bytes,
                budget,
                "requested changed path list",
            )?;
        }

        exact_files.sort_unstable();
        exact_files.dedup();
        normalize_rescan_dirs(&mut rescan_dirs);
        for path in &rescan_dirs {
            if path == &self.project_root {
                push_retained(
                    &mut rescan_prefixes,
                    String::new(),
                    |_| Ok(0),
                    budget,
                    "project-root rescan prefix",
                )?;
                continue;
            }
            if let Ok(prefix) =
                prepared_normalized_rel_path(&self.project_root, path, budget, "rescan prefix")?
            {
                push_retained(
                    &mut rescan_prefixes,
                    prefix,
                    string_backing_bytes,
                    budget,
                    "rescan prefix list",
                )?;
            }
        }
        rescan_prefixes.sort_unstable();
        rescan_prefixes.dedup();
        let mut present = self.discover_targets(exact_files, rescan_dirs, diagnostics, budget)?;
        sort_and_dedup_candidates(&mut present);

        for requested in requested_rel_paths {
            if requested.delete_unconditionally
                || (known.binary_search(&requested.rel_path).is_ok()
                    && !contains_candidate(&present, &requested.rel_path))
            {
                push_retained(
                    &mut deleted,
                    requested.rel_path,
                    |_| Ok(0),
                    budget,
                    "known deletion list",
                )?;
            }
        }
        for prefix in &rescan_prefixes {
            for known_path in known
                .iter()
                .filter(|known_path| is_path_at_or_below(known_path, prefix))
            {
                if !contains_candidate(&present, known_path) {
                    let deleted_path =
                        prepared_string_clone(known_path, budget, "known deletion path")?;
                    push_retained(
                        &mut deleted,
                        deleted_path,
                        string_backing_bytes,
                        budget,
                        "known deletion list",
                    )?;
                }
            }
        }

        Ok(ChangedDiscovery {
            present,
            deleted,
            rescan_prefixes,
        })
    }

    fn discover_targets(
        &self,
        exact_files: Vec<PathBuf>,
        rescan_dirs: Vec<PathBuf>,
        diagnostics: &mut Vec<ScanDiagnostic>,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<Vec<ScanCandidate>, ScanError> {
        if exact_files.is_empty() && rescan_dirs.is_empty() {
            return Ok(Vec::new());
        }

        let ignore_matcher = RootIgnoreMatcher::load(&self.read_root, self.options, budget)?;
        let project_root =
            retained_path_clone(&self.project_root, budget, "changed scan project root")?;
        let index_root = retained_path_clone(&self.index_root, budget, "changed scan index root")?;
        let mut builder = WalkBuilder::new(&self.project_root);
        configure_walk_builder(&mut builder);
        builder.filter_entry(move |entry: &DirEntry| {
            let path = entry.path();
            if entry.file_type().is_some_and(|kind| kind.is_symlink())
                || is_excluded_under(&project_root, path)
                || path.starts_with(&index_root)
            {
                return false;
            }
            if path == project_root {
                return true;
            }
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                return exact_files
                    .binary_search_by(|candidate| candidate.as_path().cmp(path))
                    .is_ok()
                    || rescan_dirs.iter().any(|root| path.starts_with(root));
            }
            exact_files.iter().any(|target| target.starts_with(path))
                || rescan_dirs.iter().any(|root| root.starts_with(path))
                || rescan_dirs.iter().any(|root| path.starts_with(root))
        });

        let present = self.collect_walk(builder, diagnostics, budget, ignore_matcher.as_ref())?;
        ignore_matcher.validate_current(&self.read_root, self.options)?;
        Ok(present)
    }

    fn collect_walk(
        &self,
        builder: WalkBuilder,
        diagnostics: &mut Vec<ScanDiagnostic>,
        budget: &mut AssetLoadBudget,
        ignore_matcher: &RootIgnoreMatcher,
    ) -> std::result::Result<Vec<ScanCandidate>, ScanError> {
        let mut present = Vec::new();
        let mut ignored_dirs = Vec::new();

        // `build()` is intentionally the single-threaded iterator. Every ignore-file mechanism is
        // disabled on the builder. Matching happens here so every matcher allocation is charged to
        // the caller's budget before it can occur.
        for entry in builder.build() {
            budget.consume_entries(1)?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    push_diagnostic(
                        diagnostics,
                        ScanDiagnostic::WalkFailed {
                            message: error.to_string(),
                        },
                        budget,
                    )?;
                    continue;
                }
            };
            let path = entry.path();
            if ignored_dirs.iter().any(|ignored| path.starts_with(ignored)) {
                continue;
            }
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            if path != self.project_root
                && let Ok(relative) = path.strip_prefix(&self.project_root)
                && ignore_matcher.is_ignored(relative, is_dir, budget)?
            {
                if is_dir {
                    let ignored =
                        prepared_path_clone(path, budget, "ignored directory path clone")?;
                    push_retained(
                        &mut ignored_dirs,
                        ignored,
                        path_backing_bytes,
                        budget,
                        "ignored directory list",
                    )?;
                }
                continue;
            }
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if !is_supported_asset_path(path) {
                continue;
            }
            preflight_candidate(
                &self.project_root,
                path,
                present.len(),
                present.capacity(),
                budget,
            )?;
            match self.candidate_for_path(path) {
                Ok(Some(candidate)) => {
                    push_retained(
                        &mut present,
                        candidate,
                        candidate_backing_bytes,
                        budget,
                        "scan candidate list",
                    )?;
                }
                Ok(None) => {}
                Err(diagnostic) => push_diagnostic(diagnostics, diagnostic, budget)?,
            }
        }

        Ok(present)
    }

    fn candidate_for_path(
        &self,
        path: &Path,
    ) -> std::result::Result<Option<ScanCandidate>, ScanDiagnostic> {
        if !is_supported_asset_path(path) {
            return Ok(None);
        }
        let metadata = fs::symlink_metadata(path).map_err(|error| ScanDiagnostic::ReadFailed {
            rel_path: normalized_rel_path(&self.project_root, path).unwrap_or_default(),
            part: SourcePart::Asset,
            kind: error.kind(),
            message: error.to_string(),
        })?;
        if metadata.file_type().is_symlink() {
            return Err(ScanDiagnostic::PathRejected {
                path: path.to_path_buf(),
                reason: PathRejection::Symlink,
            });
        }
        if !metadata.is_file() {
            return Err(ScanDiagnostic::PathRejected {
                path: path.to_path_buf(),
                reason: PathRejection::UnsupportedFileType,
            });
        }
        let canonical = path
            .canonicalize()
            .map_err(|error| ScanDiagnostic::ReadFailed {
                rel_path: normalized_rel_path(&self.project_root, path).unwrap_or_default(),
                part: SourcePart::Asset,
                kind: error.kind(),
                message: error.to_string(),
            })?;
        if let Err(reason) = self.validate_boundary(&canonical) {
            return Err(ScanDiagnostic::PathRejected {
                path: canonical,
                reason,
            });
        }
        let rel_path = match normalized_rel_path(&self.project_root, &canonical) {
            Ok(rel_path) => rel_path,
            Err(reason) => {
                return Err(ScanDiagnostic::PathRejected {
                    path: canonical,
                    reason,
                });
            }
        };
        let Some(name) = canonical.file_stem().and_then(|name| name.to_str()) else {
            return Err(ScanDiagnostic::PathRejected {
                path: canonical.clone(),
                reason: PathRejection::NonUtf8RelativePath,
            });
        };
        let name = name.to_owned();
        let kind = classify_kind(&canonical);

        Ok(Some(ScanCandidate {
            rel_path,
            abs_path: canonical,
            name,
            kind,
        }))
    }

    fn validate_boundary(&self, path: &Path) -> std::result::Result<(), PathRejection> {
        if !path.starts_with(&self.project_root) {
            return Err(PathRejection::OutsideProject);
        }
        if !self.scan_roots.iter().any(|root| path.starts_with(root)) {
            return Err(PathRejection::OutsideScanRoots);
        }
        if path.starts_with(&self.index_root) {
            return Err(PathRejection::InsideIndexRoot);
        }
        if is_excluded_under(&self.project_root, path) {
            return Err(PathRejection::Excluded);
        }
        Ok(())
    }

    fn open_read_handle(
        &self,
        relative: &Path,
        part: SourcePart,
    ) -> std::result::Result<Handle, ReadFailure> {
        let file = self
            .read_root
            .open_relative(relative)
            .map_err(|source| ReadFailure::Io {
                part,
                context: ReadIoContext::Open,
                source,
            })?;
        Handle::from_file(file).map_err(|source| ReadFailure::Io {
            part,
            context: ReadIoContext::Read,
            source,
        })
    }

    fn revalidate_read_blob(
        &self,
        relative: &Path,
        part: SourcePart,
        blob: &ReadBlob,
        metrics: &mut ScanMetrics,
    ) -> std::result::Result<(), ReadFailure> {
        let reopened = match self.read_root.open_relative(relative) {
            Ok(file) => Handle::from_file(file).map_err(|source| ReadFailure::Io {
                part,
                context: ReadIoContext::Read,
                source,
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ReadFailure::Changed {
                    part,
                    before: Some(blob.snapshot.hint),
                    after: None,
                });
            }
            Err(source) => {
                return Err(ReadFailure::Io {
                    part,
                    context: ReadIoContext::Open,
                    source,
                });
            }
        };
        metrics.opened = metrics.opened.saturating_add(1);
        let metadata = reopened
            .as_file()
            .metadata()
            .map_err(|source| ReadFailure::Io {
                part,
                context: ReadIoContext::Read,
                source,
            })?;
        let current = FileSnapshot::from_metadata(&metadata);
        if reopened != blob.handle || current != blob.snapshot {
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
        let file = match self.read_root.open_relative(relative) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(ReadFailure::Io {
                    part: SourcePart::Meta,
                    context: ReadIoContext::Open,
                    source,
                });
            }
            Ok(file) => file,
        };
        let handle = Handle::from_file(file).map_err(|source| ReadFailure::Io {
            part: SourcePart::Meta,
            context: ReadIoContext::Read,
            source,
        })?;
        metrics.opened = metrics.opened.saturating_add(1);
        let metadata = handle
            .as_file()
            .metadata()
            .map_err(|source| ReadFailure::Io {
                part: SourcePart::Meta,
                context: ReadIoContext::Read,
                source,
            })?;
        Err(ReadFailure::Changed {
            part: SourcePart::Meta,
            before: None,
            after: Some(file_hint(&metadata)),
        })
    }

    fn diagnose_known_paths(
        &self,
        known_rel_paths: &[String],
        diagnostics: &mut Vec<ScanDiagnostic>,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<(), ScanError> {
        for rel_path in known_rel_paths {
            if !is_portable_known_path(rel_path) {
                let path = prepared_path_clone(
                    Path::new(rel_path),
                    budget,
                    "invalid known path diagnostic",
                )?;
                push_diagnostic(
                    diagnostics,
                    ScanDiagnostic::PathRejected {
                        path,
                        reason: PathRejection::InvalidPath,
                    },
                    budget,
                )?;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ChangedDiscovery {
    present: Vec<ScanCandidate>,
    deleted: Vec<String>,
    rescan_prefixes: Vec<String>,
}

#[derive(Debug)]
struct RequestedPath {
    rel_path: String,
    delete_unconditionally: bool,
}

#[derive(Debug)]
struct ReadBlob {
    handle: Handle,
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

#[derive(Debug, Clone, Copy)]
enum ReadIoContext {
    Open,
    Read,
}

#[derive(Debug)]
enum ReadFailure {
    Io {
        part: SourcePart,
        context: ReadIoContext,
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
    Borrowed(&'path Path),
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

fn configure_walk_builder(builder: &mut WalkBuilder) {
    builder
        .follow_links(false)
        .parents(false)
        .hidden(true)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false);
}

fn read_file_once(
    mut handle: Handle,
    part: SourcePart,
    limit: u64,
    retained_limit: u64,
    metrics: &mut ScanMetrics,
    budget: &mut AssetLoadBudget,
) -> std::result::Result<ReadBlob, ReadFailure> {
    metrics.opened = metrics.opened.saturating_add(1);
    let before = handle
        .as_file()
        .metadata()
        .map_err(|source| ReadFailure::Io {
            part,
            context: ReadIoContext::Read,
            source,
        })?;
    if !before.is_file() {
        return Err(ReadFailure::NotRegularFile { part });
    }
    let before_snapshot = FileSnapshot::from_metadata(&before);
    let before_hint = before_snapshot.hint;
    if before_hint.size > limit {
        return Ok(ReadBlob {
            handle,
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

    let mut bytes = if before_hint.size <= retained_limit {
        let initial_capacity =
            usize::try_from(before_hint.size).map_err(|_| ReadFailure::Allocation {
                part,
                requested: before_hint.size,
            })?;
        budget
            .consume_bytes(before_hint.size)
            .map_err(|source| ReadFailure::Budget { part, source })?;
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
        let read = handle
            .as_file_mut()
            .read(&mut buffer)
            .map_err(|source| ReadFailure::Io {
                part,
                context: ReadIoContext::Read,
                source,
            })?;
        if read == 0 {
            break;
        }
        let read_u64 = u64::try_from(read).unwrap_or(u64::MAX);
        metrics.read_bytes = metrics.read_bytes.saturating_add(read_u64);
        observed = observed.saturating_add(read_u64);
        if observed > before_hint.size {
            let after = handle
                .as_file()
                .metadata()
                .map_err(|source| ReadFailure::Io {
                    part,
                    context: ReadIoContext::Read,
                    source,
                })?;
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

    let after = handle
        .as_file()
        .metadata()
        .map_err(|source| ReadFailure::Io {
            part,
            context: ReadIoContext::Read,
            source,
        })?;
    let after_snapshot = FileSnapshot::from_metadata(&after);
    if before_snapshot != after_snapshot || after_snapshot.hint.size != observed {
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
        handle,
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

    fn path_clone(
        &mut self,
        value: &Path,
        allocation: &'static str,
    ) -> std::result::Result<PathBuf, ScanError> {
        let requested = value.as_os_str().len();
        let requested_bytes = checked_usize_bytes(requested)?;
        self.charge_requested(requested_bytes)?;
        let mut cloned = PathBuf::new();
        cloned
            .try_reserve_exact(requested)
            .map_err(|source| ScanError::AllocationFailed {
                allocation,
                requested,
                source,
            })?;
        self.charge_actual(requested_bytes, path_backing_bytes(&cloned)?)?;
        cloned.push(value);
        Ok(cloned)
    }

    fn joined_path(
        &mut self,
        root: &Path,
        relative: &Path,
    ) -> std::result::Result<PathBuf, ScanError> {
        let requested = root
            .as_os_str()
            .len()
            .checked_add(1)
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
        self.charge_actual(requested_bytes, path_backing_bytes(&path)?)?;
        path.push(root);
        path.push(relative);
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
            checked_string_bytes(candidate.rel_path.len())?,
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
    let rel_path = materializer.string_clone(&candidate.rel_path, "read source relative path")?;
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
            Self::Borrowed(path) => checked_usize_bytes(path.as_os_str().len()),
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
            Self::Borrowed(path) => materializer.path_clone(path, "read diagnostic path"),
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
        ReadFailure::Io {
            part,
            context: ReadIoContext::Open,
            source,
        } if source.kind() == io::ErrorKind::InvalidInput => ReadDiagnosticSpec::PathRejected {
            path: diagnostic_path,
            reason: PathRejection::InvalidPath,
            part: *part,
        },
        ReadFailure::Io { part, source, .. } => ReadDiagnosticSpec::ReadFailed {
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
        source: None,
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
        ScanError::IgnoreIo { source, .. } => ReadDiagnosticSpec::ReadFailed {
            rel_path: "",
            part,
            kind: source.kind(),
            message: DiagnosticMessage::Io(source),
        },
        ScanError::IgnoreLimitExceeded { .. }
        | ScanError::IgnoreChangedDuringRead { .. }
        | ScanError::IgnoreSyntax { .. } => ReadDiagnosticSpec::ReadFailed {
            rel_path: "",
            part,
            kind: io::ErrorKind::InvalidData,
            message: DiagnosticMessage::Static("project-root ignore policy failed"),
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
    known: &[String],
    present: &[ScanCandidate],
    deleted: &mut Vec<String>,
    budget: &mut AssetLoadBudget,
) -> std::result::Result<(), ScanError> {
    for path in known {
        if contains_candidate(present, path) {
            continue;
        }
        let path = prepared_string_clone(path, budget, "known deletion path")?;
        push_retained(
            deleted,
            path,
            string_backing_bytes,
            budget,
            "known deletion list",
        )?;
    }
    Ok(())
}

fn normalize_rescan_dirs(paths: &mut Vec<PathBuf>) {
    paths.sort_unstable();
    paths.dedup();
    let mut retained = 0;
    for candidate in 0..paths.len() {
        let covered = retained > 0 && paths[candidate].starts_with(&paths[retained - 1]);
        if covered {
            continue;
        }
        paths.swap(retained, candidate);
        retained += 1;
    }
    paths.truncate(retained);
}

fn merge_changed_paths(
    present: &[ScanCandidate],
    deleted: &[String],
    budget: &mut AssetLoadBudget,
) -> std::result::Result<Vec<String>, ScanError> {
    let mut changed = Vec::new();
    let mut present_index = 0;
    let mut deleted_index = 0;
    while present_index < present.len() || deleted_index < deleted.len() {
        let next = match (present.get(present_index), deleted.get(deleted_index)) {
            (Some(candidate), Some(deleted_path)) => match candidate.rel_path.cmp(deleted_path) {
                Ordering::Less => {
                    present_index += 1;
                    candidate.rel_path.as_str()
                }
                Ordering::Equal => {
                    present_index += 1;
                    deleted_index += 1;
                    candidate.rel_path.as_str()
                }
                Ordering::Greater => {
                    deleted_index += 1;
                    deleted_path.as_str()
                }
            },
            (Some(candidate), None) => {
                present_index += 1;
                candidate.rel_path.as_str()
            }
            (None, Some(deleted_path)) => {
                deleted_index += 1;
                deleted_path.as_str()
            }
            (None, None) => break,
        };
        let next = prepared_string_clone(next, budget, "changed scan path")?;
        push_retained(
            &mut changed,
            next,
            string_backing_bytes,
            budget,
            "changed scan path list",
        )?;
    }
    Ok(changed)
}

fn sort_and_dedup_candidates(candidates: &mut Vec<ScanCandidate>) {
    candidates.sort_unstable_by(compare_candidates);
    candidates.dedup_by(|left, right| left.rel_path == right.rel_path);
}

fn compare_candidates(left: &ScanCandidate, right: &ScanCandidate) -> Ordering {
    left.rel_path
        .cmp(&right.rel_path)
        .then_with(|| left.abs_path.cmp(&right.abs_path))
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.kind.canonical_name().cmp(right.kind.canonical_name()))
}

fn contains_candidate(candidates: &[ScanCandidate], rel_path: &str) -> bool {
    candidates
        .binary_search_by(|candidate| candidate.rel_path.as_str().cmp(rel_path))
        .is_ok()
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

fn preflight_candidate(
    project_root: &Path,
    path: &Path,
    candidate_len: usize,
    candidate_capacity: usize,
    budget: &AssetLoadBudget,
) -> std::result::Result<(), ScanError> {
    budget.check_entries(1)?;
    let path_bytes = checked_usize_bytes(path.as_os_str().len())?;
    let relative_bytes = path
        .strip_prefix(project_root)
        .ok()
        .map_or(Ok(0), |relative| {
            checked_usize_bytes(relative.as_os_str().len())
        })?;
    let name_bytes = path
        .file_stem()
        .map_or(Ok(0), |name| checked_usize_bytes(name.len()))?;
    let retained_bytes =
        checked_byte_add(checked_byte_add(path_bytes, relative_bytes)?, name_bytes)?;
    let vector_bytes = if candidate_len == candidate_capacity {
        checked_vec_bytes::<ScanCandidate>(1)?
    } else {
        0
    };
    budget.check_bytes(checked_byte_add(retained_bytes, vector_bytes)?)?;
    Ok(())
}

fn preflight_changed_path(
    project_root: &Path,
    supplied: &Path,
    budget: &AssetLoadBudget,
) -> std::result::Result<(), ScanError> {
    budget.check_entries(1)?;
    let supplied_bytes = supplied.as_os_str().len();
    let requested = if supplied.is_absolute() {
        supplied_bytes
    } else {
        project_root
            .as_os_str()
            .len()
            .checked_add(1)
            .and_then(|bytes| bytes.checked_add(supplied_bytes))
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "changed scan path",
            })?
    };
    budget.check_bytes(checked_usize_bytes(requested)?)?;
    Ok(())
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

fn prepared_normalized_rel_path(
    project_root: &Path,
    path: &Path,
    budget: &AssetLoadBudget,
    allocation: &'static str,
) -> std::result::Result<std::result::Result<String, PathRejection>, ScanError> {
    let raw = match normalized_rel_str(project_root, path) {
        Ok(raw) => raw,
        Err(reason) => return Ok(Err(reason)),
    };
    let planned_bytes = checked_string_bytes(raw.len())?;
    budget.check_bytes(planned_bytes)?;
    let mut normalized = String::new();
    normalized
        .try_reserve_exact(raw.len())
        .map_err(|source| ScanError::AllocationFailed {
            allocation,
            requested: raw.len(),
            source,
        })?;
    for character in raw.chars() {
        normalized.push(if character == '\\' { '/' } else { character });
    }
    budget.check_bytes(string_backing_bytes(&normalized)?)?;
    Ok(Ok(normalized))
}

fn retained_path_clone(
    value: &Path,
    budget: &mut AssetLoadBudget,
    allocation: &'static str,
) -> std::result::Result<PathBuf, ScanError> {
    let cloned = prepared_path_clone(value, budget, allocation)?;
    let bytes = path_backing_bytes(&cloned)?;
    budget.check_bytes(bytes)?;
    budget.consume_bytes(bytes)?;
    Ok(cloned)
}

fn candidate_backing_bytes(candidate: &ScanCandidate) -> std::result::Result<u64, ScanError> {
    checked_byte_add(
        checked_byte_add(
            string_backing_bytes(&candidate.rel_path)?,
            path_backing_bytes(&candidate.abs_path)?,
        )?,
        string_backing_bytes(&candidate.name)?,
    )
}

fn requested_path_backing_bytes(requested: &RequestedPath) -> std::result::Result<u64, ScanError> {
    string_backing_bytes(&requested.rel_path)
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

fn normalized_rel_path(
    project_root: &Path,
    path: &Path,
) -> std::result::Result<String, PathRejection> {
    Ok(normalized_rel_str(project_root, path)?.replace('\\', "/"))
}

fn normalized_rel_str<'path>(
    project_root: &Path,
    path: &'path Path,
) -> std::result::Result<&'path str, PathRejection> {
    let rel = path
        .strip_prefix(project_root)
        .map_err(|_| PathRejection::OutsideProject)?;
    let rel = rel.to_str().ok_or(PathRejection::NonUtf8RelativePath)?;
    if rel.is_empty() {
        return Err(PathRejection::InvalidPath);
    }
    Ok(rel)
}

fn normalized_rel_path_matches(raw: &str, expected: &str) -> bool {
    raw.len() == expected.len()
        && raw
            .bytes()
            .zip(expected.bytes())
            .all(|(left, right)| if left == b'\\' { b'/' } else { left } == right)
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

fn read_canonical_path_backing_bound(path: &PathBuf) -> std::result::Result<u64, ScanError> {
    let capacity = path
        .capacity()
        .checked_add(READ_CANONICAL_PATH_EXTRA_CAPACITY)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "read canonical path backing",
        })?;
    checked_usize_bytes(capacity)
}

fn is_portable_known_path(path: &str) -> bool {
    if path.is_empty() || path.contains('\\') {
        return false;
    }
    Path::new(path)
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
}

fn absolute_from(project_root: &Path, path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    };
    normalize_absolute(&absolute)
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

fn resolve_allow_missing(path: &Path) -> io::Result<PathBuf> {
    let normalized = normalize_absolute(path)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let mut cursor = normalized.clone();
    let mut tail = Vec::<OsString>::new();
    loop {
        match cursor.canonicalize() {
            Ok(mut canonical) => {
                for component in tail.into_iter().rev() {
                    canonical.push(component);
                }
                return normalize_absolute(&canonical).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(component) = cursor.file_name().map(OsString::from) else {
                    return Err(error);
                };
                if !cursor.pop() {
                    return Err(error);
                }
                tail.push(component);
            }
            Err(error) => return Err(error),
        }
    }
}

fn canonical_directory(path: &Path) -> io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    if !canonical.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a directory: {}", path.display()),
        ));
    }
    Ok(canonical)
}

fn path_error_reason(error: &io::Error) -> PathRejection {
    if error.kind() == io::ErrorKind::InvalidInput {
        PathRejection::InvalidPath
    } else {
        PathRejection::UnsupportedFileType
    }
}

fn is_path_at_or_below(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn is_supported_asset_path(path: &Path) -> bool {
    !is_meta_path(path) && !is_hidden_file(path) && path.file_name().is_some()
}

fn is_hidden_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

fn is_excluded_under(project_root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(project_root) else {
        return false;
    };
    relative.components().any(|component| {
        let Component::Normal(name) = component else {
            return false;
        };
        name.to_str().is_some_and(|name| {
            matches!(
                name,
                ".git"
                    | "target"
                    | "Library"
                    | ".venv-unitypy"
                    | ".unity-asset-search"
                    | "unity-asset-search"
                    | "Temp"
                    | "Obj"
                    | "Logs"
            )
        })
    })
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
        scanner
            .plan(intent, known, &mut AssetLoadBudget::default())
            .unwrap()
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
        read_canonical_path_backing_bound(&candidate.abs_path).unwrap()
            + u64::try_from(
                meta_relative_path_capacity_bound(Path::new(&candidate.rel_path)).unwrap(),
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
                .map(|candidate| candidate.rel_path.as_str())
                .collect::<Vec<_>>(),
            ["Assets/A.asset", "Assets/Z/B.asset"]
        );
        assert_eq!(plan.deleted, ["Assets/Removed.asset"]);
        assert_eq!(
            plan.changed,
            ["Assets/A.asset", "Assets/Removed.asset", "Assets/Z/B.asset"]
        );
        assert_eq!(plan.metrics.discovered, 2);
        assert_eq!(plan.metrics.deleted, 1);
    }

    #[test]
    fn project_root_ignore_rules_are_ordered_and_nested_files_are_not_loaded() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets/Nested")).unwrap();
        fs::write(
            project.path().join(".gitignore"),
            b"Assets/*.asset\n!Assets/Keep.asset\n",
        )
        .unwrap();
        fs::write(
            project.path().join("Assets/Nested/.gitignore"),
            b"*.asset\n",
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
                .map(|candidate| candidate.rel_path.as_str())
                .collect::<Vec<_>>(),
            ["Assets/Keep.asset", "Assets/Nested/Visible.asset"]
        );
    }

    #[test]
    fn ignored_directories_prune_whitelisted_descendants() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets/Generated")).unwrap();
        fs::write(
            project.path().join(".ignore"),
            b"Assets/Generated/\n!Assets/Generated/Keep.asset\n",
        )
        .unwrap();
        for path in ["Assets/Generated/Drop.asset", "Assets/Generated/Keep.asset"] {
            fs::write(project.path().join(path), b"asset").unwrap();
        }
        let scanner = scanner(&project, ScanReadLimits::default());

        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        assert!(plan.present.is_empty());
    }

    #[test]
    fn disabling_project_root_ignore_files_makes_rules_inert() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::write(project.path().join(".gitignore"), b"Assets/Hidden.asset\n").unwrap();
        fs::write(project.path().join("Assets/Hidden.asset"), b"asset").unwrap();
        let scanner = scanner_with_options(
            &project,
            SearchIndexOptions {
                respect_project_root_ignore_files: false,
                ..SearchIndexOptions::default()
            },
            ScanReadLimits::default(),
        );

        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);

        assert_eq!(plan.present.len(), 1);
        assert_eq!(plan.present[0].rel_path, "Assets/Hidden.asset");
    }

    #[test]
    fn project_root_ignore_file_and_line_limits_accept_exact_and_reject_one_over() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::write(project.path().join("Assets/Hidden.asset"), b"asset").unwrap();
        let encoded = b"Assets/Hidden.asset\n";
        fs::write(project.path().join(".ignore"), encoded).unwrap();
        let line_bytes = encoded.len() - 1;
        let exact_options = SearchIndexOptions {
            respect_project_root_gitignore: false,
            max_project_root_ignore_file_bytes: encoded.len() as u64,
            max_project_root_ignore_line_bytes: line_bytes,
            max_project_root_ignore_patterns: 1,
            max_project_root_ignore_parser_work: (encoded.len() as u64) * 2,
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
                max_project_root_ignore_file_bytes: (encoded.len() - 1) as u64,
                max_project_root_ignore_line_bytes: line_bytes - 1,
                ..exact_options
            },
            ScanReadLimits::default(),
        )
        .plan(ScanIntent::Full, &[], &mut AssetLoadBudget::default())
        .unwrap_err();
        assert!(matches!(
            file_error,
            ScanError::IgnoreLimitExceeded {
                file: ".ignore",
                resource: IgnoreLimitResource::FileBytes,
                observed_at_least,
                limit,
            } if observed_at_least == encoded.len() as u64
                && limit == (encoded.len() - 1) as u64
        ));

        let line_error = scanner_with_options(
            &project,
            SearchIndexOptions {
                max_project_root_ignore_line_bytes: line_bytes - 1,
                ..exact_options
            },
            ScanReadLimits::default(),
        )
        .plan(ScanIntent::Full, &[], &mut AssetLoadBudget::default())
        .unwrap_err();
        assert!(matches!(
            line_error,
            ScanError::IgnoreLimitExceeded {
                file: ".ignore",
                resource: IgnoreLimitResource::LineBytes,
                observed_at_least,
                limit,
            } if observed_at_least == line_bytes as u64
                && limit == (line_bytes - 1) as u64
        ));
    }

    #[test]
    fn project_root_ignore_pattern_limit_accepts_exact_and_rejects_one_over() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        let encoded = b"Assets/A.asset\nAssets/B.asset\n";
        fs::write(project.path().join(".ignore"), encoded).unwrap();
        let exact_options = SearchIndexOptions {
            respect_project_root_gitignore: false,
            max_project_root_ignore_file_bytes: encoded.len() as u64,
            max_project_root_ignore_line_bytes: "Assets/A.asset".len(),
            max_project_root_ignore_patterns: 2,
            max_project_root_ignore_parser_work: (encoded.len() as u64) * 2,
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
                max_project_root_ignore_patterns: 1,
                ..exact_options
            },
            ScanReadLimits::default(),
        )
        .plan(ScanIntent::Full, &[], &mut AssetLoadBudget::default())
        .unwrap_err();
        assert!(matches!(
            error,
            ScanError::IgnoreLimitExceeded {
                file: "project-root ignore policy",
                resource: IgnoreLimitResource::Patterns,
                observed_at_least: 2,
                limit: 1,
            }
        ));
    }

    #[test]
    fn oversized_project_root_ignore_file_fails_before_walking_assets() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::write(project.path().join("Assets/Visible.asset"), b"asset").unwrap();
        fs::write(project.path().join(".ignore"), vec![b'a'; 1025]).unwrap();
        let scanner = scanner_with_options(
            &project,
            SearchIndexOptions {
                respect_project_root_gitignore: false,
                max_project_root_ignore_file_bytes: 1024,
                max_project_root_ignore_line_bytes: 1024,
                max_project_root_ignore_parser_work: 2048,
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
            ScanError::IgnoreLimitExceeded {
                file: ".ignore",
                resource: IgnoreLimitResource::FileBytes,
                observed_at_least: 1025,
                limit: 1024,
            }
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
        fs::write(&ignore_path, b"Assets/Ignored.asset\n").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let before = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
        let known = before
            .present
            .iter()
            .map(|candidate| candidate.rel_path.clone())
            .collect::<Vec<_>>();
        fs::write(&ignore_path, b"Assets/Keep.asset\n").unwrap();

        let changed = plan_with_default_budget(
            &scanner,
            ScanIntent::ChangedPaths(vec![ignore_path]),
            &known,
        );
        let full = plan_with_default_budget(&scanner, ScanIntent::Full, &known);

        assert_eq!(
            changed
                .present
                .iter()
                .map(|candidate| candidate.rel_path.as_str())
                .collect::<Vec<_>>(),
            full.present
                .iter()
                .map(|candidate| candidate.rel_path.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(changed.deleted, full.deleted);
        assert_eq!(changed.present[0].rel_path, "Assets/Ignored.asset");
        assert_eq!(changed.deleted, ["Assets/Keep.asset"]);
        assert_eq!(changed.rescan_prefixes, ["Assets"]);
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
            .map(|candidate| candidate.rel_path.clone())
            .collect::<Vec<_>>();
        fs::write(&ignore_path, b"Root.asset\n").unwrap();

        let changed = plan_with_default_budget(
            &scanner,
            ScanIntent::ChangedPaths(vec![ignore_path]),
            &known,
        );
        let full = plan_with_default_budget(&scanner, ScanIntent::Full, &known);

        assert_eq!(changed.present, full.present);
        assert_eq!(changed.deleted, full.deleted);
        assert!(changed.present.is_empty());
        assert_eq!(changed.deleted, ["Root.asset"]);
        assert_eq!(changed.rescan_prefixes, [""]);
    }

    #[cfg(windows)]
    #[test]
    fn changed_root_ignore_policy_with_case_variant_converges_to_a_full_scan() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        let asset_path = project.path().join("Assets/Hidden.asset");
        let ignore_path = project.path().join(".UNITY-ASSET-SEARCH-IGNORE");
        fs::write(&asset_path, b"asset").unwrap();
        fs::write(&ignore_path, b"").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let before = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
        let known = before
            .present
            .iter()
            .map(|candidate| candidate.rel_path.clone())
            .collect::<Vec<_>>();
        fs::write(&ignore_path, b"Assets/Hidden.asset\n").unwrap();

        let changed = plan_with_default_budget(
            &scanner,
            ScanIntent::ChangedPaths(vec![ignore_path]),
            &known,
        );
        let full = plan_with_default_budget(&scanner, ScanIntent::Full, &known);

        assert_eq!(changed.present, full.present);
        assert_eq!(changed.deleted, full.deleted);
        assert!(changed.present.is_empty());
        assert_eq!(changed.deleted, ["Assets/Hidden.asset"]);
        assert_eq!(changed.rescan_prefixes, ["Assets"]);
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
                ScanIntent::ChangedPaths(vec![PathBuf::from("Assets/Missing.asset")]),
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
    fn changed_path_diagnostics_have_stable_structural_order() {
        let project = tempfile::tempdir().unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let outside = tempfile::tempdir().unwrap();
        let first = outside.path().join("A.asset");
        let second = outside.path().join("Z.asset");

        let left = plan_with_default_budget(
            &scanner,
            ScanIntent::ChangedPaths(vec![second.clone(), first.clone()]),
            &[],
        );
        let right =
            plan_with_default_budget(&scanner, ScanIntent::ChangedPaths(vec![first, second]), &[]);

        assert_eq!(left.diagnostics, right.diagnostics);
        assert_eq!(left.changed, right.changed);
        assert_eq!(left.deleted, right.deleted);
        assert_eq!(left.rescan_prefixes, right.rescan_prefixes);
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

        assert_eq!(plan.deleted, ["Packages/Old.asset"]);
        assert_eq!(plan.present[0].rel_path, "Assets/Current.asset");
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
        let source = first.source.unwrap();
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
        assert!(second.source.unwrap().unchanged);
        assert_eq!(budget.usage().entries, 2);
    }

    #[test]
    fn read_source_rejects_without_allocating_diagnostics_after_budget_is_exhausted() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        fs::write(project.path().join("Assets/Budgeted.asset"), b"asset").unwrap();
        let scanner = scanner(&project, ScanReadLimits::default());
        let plan = plan_with_default_budget(&scanner, ScanIntent::Full, &[]);
        let canonical_bytes = read_canonical_path_backing_bound(&plan.present[0].abs_path).unwrap();
        let meta_path_bytes = u64::try_from(
            meta_relative_path_capacity_bound(Path::new(&plan.present[0].rel_path)).unwrap(),
        )
        .unwrap();
        let max_bytes = canonical_bytes
            .checked_add(meta_path_bytes)
            .and_then(|bytes| bytes.checked_add(5))
            .unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let outcome = scanner.read_source(&plan.present[0], None, &mut budget);

        assert!(outcome.source.is_none());
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
            read_canonical_path_backing_bound(&plan.present[0].abs_path).unwrap()
                > diagnostic_bytes
        );
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: diagnostic_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let outcome = scanner.read_source(&plan.present[0], None, &mut budget);

        assert!(outcome.source.is_none());
        assert!(matches!(
            outcome.diagnostics.as_slice(),
            [ScanDiagnostic::BudgetExceeded {
                rel_path,
                part: SourcePart::Asset,
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

        assert!(outcome.source.is_none());
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

        assert!(outcome.source.is_none());
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

        let source = outcome.source.as_ref().unwrap();
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
        let source = outcome.source.as_ref().unwrap();

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
        let source = outcome.source.unwrap();

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
        let handle = scanner
            .open_read_handle(relative, SourcePart::Asset)
            .unwrap();
        let mut metrics = ScanMetrics::default();
        let mut budget = AssetLoadBudget::default();
        let blob = read_file_once(
            handle,
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

        let plan =
            plan_with_default_budget(&scanner, ScanIntent::ChangedPaths(vec![directory]), &known);

        assert_eq!(plan.rescan_prefixes, ["Assets/Area"]);
        assert_eq!(plan.deleted, ["Assets/Area/Removed.asset"]);
        assert_eq!(
            plan.present
                .iter()
                .map(|candidate| candidate.rel_path.as_str())
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
            ScanIntent::ChangedPaths(vec![PathBuf::from("Assets/Removed")]),
            &known,
        );

        assert_eq!(plan.rescan_prefixes, ["Assets/Removed"]);
        assert_eq!(
            plan.deleted,
            [
                "Assets/Removed",
                "Assets/Removed/A.asset",
                "Assets/Removed/Nested/B.asset"
            ]
        );
        assert!(plan.present.is_empty());
    }

    #[test]
    fn custom_index_root_inside_scan_root_is_excluded() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets/SearchIndex")).unwrap();
        fs::write(
            project.path().join("Assets/SearchIndex/Internal.asset"),
            b"internal",
        )
        .unwrap();
        fs::write(project.path().join("Assets/Public.asset"), b"public").unwrap();
        let paths = IndexPaths::for_project(
            project.path().to_path_buf(),
            Some(project.path().join("Assets/SearchIndex")),
            None,
        )
        .unwrap();
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
                .map(|candidate| candidate.rel_path.as_str())
                .collect::<Vec<_>>(),
            ["Assets/Public.asset"]
        );
    }

    #[test]
    fn symlink_escape_is_rejected_from_changed_paths() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("Assets")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("Outside.asset");
        fs::write(&outside_file, b"outside").unwrap();
        let link = project.path().join("Assets/Escape.asset");
        if create_file_symlink(&outside_file, &link).is_err() {
            return;
        }
        let scanner = scanner(&project, ScanReadLimits::default());

        let plan = plan_with_default_budget(&scanner, ScanIntent::ChangedPaths(vec![link]), &[]);

        assert!(plan.present.is_empty());
        assert!(plan.diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic,
                ScanDiagnostic::PathRejected {
                    reason: PathRejection::OutsideProject | PathRejection::Symlink,
                    ..
                }
            )
        }));
    }

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }
}
