use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use unity_asset_core::DigestV1;
use unity_asset_search_local::PrivateIndexRootV1;
use unity_asset_search_protocol::{MAX_STATUS_SCAN_ROOTS, PortablePath, ProjectId, StatusResponse};

use crate::path_semantics::{
    ProjectPathError, ProjectPathSpace, compare_portable_paths,
    strip_prefix as strip_platform_prefix,
};
use crate::project_root::{ProjectRootAuthority, ResolveBoundProjectPathError};
use crate::scan::ScanReadLimits;

const SCAN_ENTRIES_HARD_MAX: u64 = 16_000_000;
const SCAN_PATH_BYTES_HARD_MAX: u64 = 16 * 1024 * 1024 * 1024;
const SCAN_DEPTH_HARD_MAX: u32 = 1_024;
const SCAN_DIRECTORIES_HARD_MAX: u64 = 4_000_000;
const SCAN_FILES_HARD_MAX: u64 = 16_000_000;
const SCAN_DIAGNOSTICS_HARD_MAX: u64 = 100_000;
const SCAN_POLICY_MATCHES_HARD_MAX: u64 = 256_000_000;
const SEARCH_IGNORE_FILE_BYTES_HARD_MAX: u64 = 1024 * 1024;
const SEARCH_IGNORE_LINE_BYTES_HARD_MAX: usize = 16 * 1024;
const SEARCH_IGNORE_RULES_HARD_MAX: usize = 1_024;
const SEARCH_IGNORE_PARSER_WORK_HARD_MAX: u64 = 2 * SEARCH_IGNORE_FILE_BYTES_HARD_MAX;
const SEARCH_IGNORE_AUTOMATON_BYTES_HARD_MAX: u64 = 128 * 1024 * 1024;
const SEARCH_IGNORE_COMPILATION_BYTES_HARD_MAX: u64 = 256 * 1024 * 1024;
const LOGICAL_INDEX_CONFIGURATION_VERSION: u16 = 1;

fn ensure_positive_at_most(name: &str, value: u64, hard_max: u64) -> Result<()> {
    ensure!(
        value > 0 && value <= hard_max,
        "{name} must be between 1 and {hard_max}"
    );
    Ok(())
}

#[derive(Debug, Clone)]
pub struct IndexPaths {
    project: ProjectRootAuthority,
    index_root: PrivateIndexRootV1,
    index_namespace_exclusion: Option<PathBuf>,
    scan_roots: Vec<PathBuf>,
    logical_scan_roots: Vec<String>,
}

impl IndexPaths {
    pub fn for_project(
        project_root: PathBuf,
        index_root: Option<PathBuf>,
        scan_roots: Option<Vec<PathBuf>>,
    ) -> Result<Self> {
        let project = ProjectRootAuthority::open(project_root)?;
        let canonical_project_root = project.root();
        let project_identity = project.identity();

        let scan_roots = match scan_roots {
            Some(roots) if !roots.is_empty() => roots,
            _ => default_scan_roots(canonical_project_root),
        };
        let normalized_scan_roots = normalize_scan_roots(&project, scan_roots)?;
        let (canonical_scan_roots, logical_scan_roots): (Vec<PathBuf>, Vec<String>) =
            normalized_scan_roots.into_iter().unzip();

        let index_root = match index_root {
            Some(index_root) => {
                ensure!(
                    index_root.is_absolute(),
                    "explicit private index root must be absolute: {}",
                    index_root.display()
                );
                PrivateIndexRootV1::open_or_create_for_project_override(
                    project_identity,
                    index_root,
                )
                .context("open the configured private search generation root")?
            }
            None => PrivateIndexRootV1::for_project(project_identity)
                .context("open the default private search generation root")?,
        };
        let index_root_path = index_root.path().to_path_buf();
        index_root
            .revalidate()
            .context("revalidate the private index authority before topology validation")?;
        let canonical_index_namespace = std::fs::canonicalize(index_root.namespace_path())
            .context("canonicalize the private index namespace")?;
        let canonical_index_root = std::fs::canonicalize(index_root.path())
            .context("canonicalize the project-bound private index root")?;
        index_root
            .revalidate()
            .context("revalidate the private index authority after topology validation")?;
        ensure!(
            !platform_paths_equal(canonical_project_root, &canonical_index_root),
            "project-bound private index root cannot equal the project root: {}",
            canonical_project_root.display()
        );

        let index_namespace_exclusion =
            match strip_platform_prefix(canonical_project_root, &canonical_index_namespace) {
                Ok(relative) if relative.as_os_str().is_empty() => {
                    return Err(anyhow!(
                        "private index namespace cannot equal the project root: {}",
                        canonical_project_root.display()
                    ));
                }
                Ok(relative) => Some(canonical_project_root.join(relative)),
                Err(()) => None,
            };
        if let Some(namespace) = index_namespace_exclusion.as_deref() {
            let canonical_namespace_relative =
                strip_platform_prefix(canonical_project_root, namespace)
                    .expect("canonical namespace exclusion is project relative");
            let canonical_namespace = canonical_project_root.join(canonical_namespace_relative);
            for scan_root in &canonical_scan_roots {
                ensure!(
                    strip_platform_prefix(&canonical_namespace, scan_root).is_err(),
                    "scan root cannot equal or be contained by the private index namespace: {}",
                    scan_root.display()
                );
            }
        } else {
            for scan_root in &canonical_scan_roots {
                ensure!(
                    !platform_paths_overlap(&canonical_index_root, scan_root),
                    "scan root cannot overlap the project-bound private index root when its \
                     namespace is not safely excludable: {}",
                    scan_root.display()
                );
            }
        }
        validate_status_path_contract(
            canonical_project_root,
            &index_root_path,
            &canonical_scan_roots,
        )?;
        project
            .revalidate()
            .context("revalidate project authority after index topology validation")?;

        Ok(Self {
            project,
            index_root,
            index_namespace_exclusion,
            scan_roots: canonical_scan_roots,
            logical_scan_roots,
        })
    }

    #[must_use]
    pub fn project_root(&self) -> &Path {
        self.project.root()
    }

    #[must_use]
    pub fn project_path_space(&self) -> &ProjectPathSpace {
        self.project.path_space()
    }

    #[must_use]
    pub fn project_id(&self) -> ProjectId {
        self.project.project_id()
    }

    /// Proves that the configured project path still names the retained project authority.
    ///
    /// This never rebinds the authority. Scans continue to read through the original directory
    /// handle even when this check rejects a replaced lexical path.
    pub fn revalidate_project_root(&self) -> Result<()> {
        self.project.revalidate()
    }

    #[must_use]
    pub fn index_root(&self) -> &Path {
        self.index_root.path()
    }

    /// Returns the complete private index namespace that contains project-specific index roots.
    ///
    /// This path may be an ancestor of the project root and must not be used directly as a source
    /// exclusion. Scanners and watchers should use [`Self::index_namespace_exclusion`] instead.
    #[must_use]
    pub fn index_namespace_root(&self) -> &Path {
        self.index_root.namespace_path()
    }

    /// Returns the private index namespace only when it is nested below the project root.
    ///
    /// An ancestor namespace may contain the project itself and therefore must never be used as
    /// an event filter or source exclusion.
    #[must_use]
    pub fn index_namespace_exclusion(&self) -> Option<&Path> {
        self.index_namespace_exclusion.as_deref()
    }

    pub(crate) fn private_index_root(&self) -> &PrivateIndexRootV1 {
        &self.index_root
    }

    pub(crate) fn project_authority(&self) -> &ProjectRootAuthority {
        &self.project
    }

    #[must_use]
    pub fn scan_roots(&self) -> &[PathBuf] {
        &self.scan_roots
    }

    pub(crate) fn logical_configuration_digest(
        &self,
        options: SearchIndexOptions,
    ) -> Result<DigestV1> {
        options.logical_digest(&self.logical_scan_roots)
    }
}

fn validate_status_path_contract(
    project_root: &Path,
    generation_root: &Path,
    scan_roots: &[PathBuf],
) -> Result<()> {
    let project_root = PortablePath::try_from(project_root)
        .context("project root cannot be represented by the status protocol")?;
    let generation_root = PortablePath::try_from(generation_root)
        .context("generation root cannot be represented by the status protocol")?;
    let scan_roots = scan_roots
        .iter()
        .map(|root| {
            PortablePath::try_from(root.as_path()).with_context(|| {
                format!(
                    "scan root cannot be represented by the status protocol: {}",
                    root.display()
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    StatusResponse::validate_paths(&project_root, &generation_root, &scan_roots)
        .context("configured paths exceed the status protocol response budget")
}

/// Independent limits for one deterministic project traversal.
///
/// These counters describe traversal and policy-matcher work, not retained Rust allocations. The
/// caller's [`unity_asset_core::AssetLoadBudget`] still accounts for every retained path,
/// candidate, and diagnostic allocation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScanTraversalLimits {
    pub max_entries: u64,
    pub max_path_bytes: u64,
    pub max_depth: u32,
    pub max_directories: u64,
    pub max_files: u64,
    pub max_diagnostics: u64,
    /// Maximum overlapping SearchIgnoreV1 matches evaluated during one scan.
    pub max_policy_matches: u64,
}

impl ScanTraversalLimits {
    fn validate(self) -> Result<Self> {
        ensure_positive_at_most("scan entries", self.max_entries, SCAN_ENTRIES_HARD_MAX)?;
        ensure_positive_at_most(
            "scan path bytes",
            self.max_path_bytes,
            SCAN_PATH_BYTES_HARD_MAX,
        )?;
        ensure!(
            self.max_depth <= SCAN_DEPTH_HARD_MAX,
            "scan depth must be at most {SCAN_DEPTH_HARD_MAX}"
        );
        ensure_positive_at_most(
            "scan directories",
            self.max_directories,
            SCAN_DIRECTORIES_HARD_MAX,
        )?;
        ensure_positive_at_most("scan files", self.max_files, SCAN_FILES_HARD_MAX)?;
        ensure_positive_at_most(
            "scan diagnostics",
            self.max_diagnostics,
            SCAN_DIAGNOSTICS_HARD_MAX,
        )?;
        ensure_positive_at_most(
            "scan policy matches",
            self.max_policy_matches,
            SCAN_POLICY_MATCHES_HARD_MAX,
        )?;
        Ok(self)
    }
}

impl Default for ScanTraversalLimits {
    fn default() -> Self {
        Self {
            max_entries: 4_000_000,
            max_path_bytes: 2 * 1024 * 1024 * 1024,
            max_depth: 256,
            max_directories: 1_000_000,
            max_files: 4_000_000,
            max_diagnostics: 10_000,
            max_policy_matches: 64_000_000,
        }
    }
}

/// Resource limits for the root `.unity-asset-search-ignore` policy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchIgnoreV1Limits {
    pub max_file_bytes: u64,
    pub max_line_bytes: usize,
    pub max_rules: usize,
    pub max_parser_work: u64,
    pub max_automaton_bytes: u64,
    pub max_compilation_bytes: u64,
}

impl SearchIgnoreV1Limits {
    fn validate(self) -> Result<Self> {
        ensure_positive_at_most(
            "SearchIgnoreV1 file bytes",
            self.max_file_bytes,
            SEARCH_IGNORE_FILE_BYTES_HARD_MAX,
        )?;
        ensure!(
            self.max_line_bytes > 0 && self.max_line_bytes <= SEARCH_IGNORE_LINE_BYTES_HARD_MAX,
            "SearchIgnoreV1 line bytes must be between 1 and \
             {SEARCH_IGNORE_LINE_BYTES_HARD_MAX}"
        );
        ensure!(
            u64::try_from(self.max_line_bytes).is_ok_and(|line| line <= self.max_file_bytes),
            "SearchIgnoreV1 line bytes cannot exceed file bytes"
        );
        ensure!(
            self.max_rules > 0 && self.max_rules <= SEARCH_IGNORE_RULES_HARD_MAX,
            "SearchIgnoreV1 rules must be between 1 and {SEARCH_IGNORE_RULES_HARD_MAX}"
        );
        ensure_positive_at_most(
            "SearchIgnoreV1 parser work",
            self.max_parser_work,
            SEARCH_IGNORE_PARSER_WORK_HARD_MAX,
        )?;
        ensure_positive_at_most(
            "SearchIgnoreV1 automaton bytes",
            self.max_automaton_bytes,
            SEARCH_IGNORE_AUTOMATON_BYTES_HARD_MAX,
        )?;
        ensure_positive_at_most(
            "SearchIgnoreV1 compilation bytes",
            self.max_compilation_bytes,
            SEARCH_IGNORE_COMPILATION_BYTES_HARD_MAX,
        )?;
        ensure!(
            usize::try_from(self.max_file_bytes).is_ok(),
            "SearchIgnoreV1 file bytes are not addressable on this platform"
        );
        ensure!(
            usize::try_from(self.max_automaton_bytes).is_ok(),
            "SearchIgnoreV1 automaton bytes are not addressable on this platform"
        );
        ensure!(
            usize::try_from(self.max_compilation_bytes).is_ok(),
            "SearchIgnoreV1 compilation bytes are not addressable on this platform"
        );
        Ok(self)
    }
}

impl Default for SearchIgnoreV1Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: 256 * 1024,
            max_line_bytes: 8 * 1024,
            max_rules: 1_024,
            max_parser_work: 2 * 256 * 1024,
            max_automaton_bytes: 32 * 1024 * 1024,
            max_compilation_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchIndexOptions {
    pub index_bundle_container_entries: bool,
    pub max_bundle_container_entries_per_bundle: usize,
    pub max_references_per_asset: usize,
    pub scan_limits: ScanTraversalLimits,
    pub ignore_limits: SearchIgnoreV1Limits,
    pub max_source_bytes: u64,
    pub max_retained_source_bytes: u64,
    pub max_metadata_bytes: u64,
    pub retain_previous_generations: usize,
}

impl SearchIndexOptions {
    pub(crate) fn validate(self) -> Result<Self> {
        ensure!(
            self.max_source_bytes > 0,
            "max_source_bytes must be positive"
        );
        ensure!(
            self.max_retained_source_bytes > 0,
            "max_retained_source_bytes must be positive"
        );
        ensure!(
            self.max_retained_source_bytes <= self.max_source_bytes,
            "max_retained_source_bytes cannot exceed max_source_bytes"
        );
        ensure!(
            self.max_metadata_bytes > 0,
            "max_metadata_bytes must be positive"
        );
        ensure!(
            !self.index_bundle_container_entries
                || self.max_bundle_container_entries_per_bundle > 0,
            "bundle container indexing requires a positive entry limit"
        );
        ensure!(
            self.max_references_per_asset > 0,
            "max_references_per_asset must be positive"
        );
        self.scan_limits.validate()?;
        self.ignore_limits.validate()?;
        ensure!(
            self.retain_previous_generations <= 32,
            "retain_previous_generations cannot exceed 32"
        );
        Ok(self)
    }

    fn logical_digest(self, scan_roots: &[String]) -> Result<DigestV1> {
        let logical = LogicalIndexConfigurationV1 {
            identity_version: LOGICAL_INDEX_CONFIGURATION_VERSION,
            scan_roots,
            options: LogicalSearchIndexOptions::from(self),
        };
        serde_json::to_vec(&logical)
            .map(|bytes| DigestV1::hash_bytes(&bytes))
            .map_err(|error| anyhow!("serialize logical search configuration: {error}"))
    }

    pub(crate) const fn scan_limits(self) -> ScanReadLimits {
        ScanReadLimits {
            max_asset_bytes: self.max_source_bytes,
            max_retained_asset_bytes: self.max_retained_source_bytes,
            max_meta_bytes: self.max_metadata_bytes,
        }
    }
}

#[derive(Serialize)]
struct LogicalIndexConfigurationV1<'paths> {
    identity_version: u16,
    scan_roots: &'paths [String],
    options: LogicalSearchIndexOptions,
}

#[derive(Serialize)]
struct LogicalSearchIndexOptions {
    index_bundle_container_entries: bool,
    max_bundle_container_entries_per_bundle: usize,
    max_references_per_asset: usize,
    scan_limits: ScanTraversalLimits,
    ignore_limits: SearchIgnoreV1Limits,
    max_source_bytes: u64,
    max_retained_source_bytes: u64,
    max_metadata_bytes: u64,
}

impl From<SearchIndexOptions> for LogicalSearchIndexOptions {
    fn from(options: SearchIndexOptions) -> Self {
        Self {
            index_bundle_container_entries: options.index_bundle_container_entries,
            max_bundle_container_entries_per_bundle: options
                .max_bundle_container_entries_per_bundle,
            max_references_per_asset: options.max_references_per_asset,
            scan_limits: options.scan_limits,
            ignore_limits: options.ignore_limits,
            max_source_bytes: options.max_source_bytes,
            max_retained_source_bytes: options.max_retained_source_bytes,
            max_metadata_bytes: options.max_metadata_bytes,
        }
    }
}

impl Default for SearchIndexOptions {
    fn default() -> Self {
        Self {
            index_bundle_container_entries: false,
            max_bundle_container_entries_per_bundle: 50_000,
            max_references_per_asset: 100_000,
            scan_limits: ScanTraversalLimits::default(),
            ignore_limits: SearchIgnoreV1Limits::default(),
            max_source_bytes: 2 * 1024 * 1024 * 1024,
            max_retained_source_bytes: 64 * 1024 * 1024,
            max_metadata_bytes: 8 * 1024 * 1024,
            retain_previous_generations: 2,
        }
    }
}

fn default_scan_roots(project_root: &Path) -> Vec<PathBuf> {
    let assets = project_root.join("Assets");
    if !is_ordinary_directory(&assets) {
        return vec![project_root.to_path_buf()];
    }

    let roots = ["Assets", "Packages", "ProjectSettings"]
        .into_iter()
        .map(|directory| project_root.join(directory))
        .filter(|candidate| is_ordinary_directory(candidate))
        .collect::<Vec<_>>();
    if roots.is_empty() {
        vec![project_root.to_path_buf()]
    } else {
        roots
    }
}

fn is_ordinary_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn normalize_scan_roots(
    project: &ProjectRootAuthority,
    roots: Vec<PathBuf>,
) -> Result<Vec<(PathBuf, String)>> {
    ensure!(
        roots.len() <= MAX_STATUS_SCAN_ROOTS,
        "scan root count {} exceeds the protocol maximum of {MAX_STATUS_SCAN_ROOTS}",
        roots.len()
    );
    let project_root = project.root();
    let mut normalized = Vec::new();
    for root in roots {
        let root = if root.is_absolute() {
            root
        } else {
            project_root.join(root)
        };
        let bound = project
            .resolve_existing_directory(&root)
            .map_err(|error| match error {
                ResolveBoundProjectPathError::Path(ProjectPathError::OutsideProject { .. }) => {
                    anyhow!("scan root must be inside project root: {}", root.display())
                }
                ResolveBoundProjectPathError::Path(ProjectPathError::InvalidComponent {
                    ..
                }) => {
                    anyhow!(
                        "scan root contains a non-portable component: {}",
                        root.display()
                    )
                }
                ResolveBoundProjectPathError::Path(error) => anyhow::Error::new(error)
                    .context(format!("validate scan root path: {}", root.display())),
                ResolveBoundProjectPathError::Filesystem(error) => anyhow::Error::new(error)
                    .context(format!(
                        "open scan root without following links: {}",
                        root.display()
                    )),
            })?;
        normalized.push((project_root.join(bound.relative_path()), bound.identity()));
    }
    normalized.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let mut identities = Vec::with_capacity(normalized.len());
    normalized.retain(|(_, identity)| {
        if identities.contains(identity) {
            false
        } else {
            identities.push(*identity);
            true
        }
    });
    let mut candidates = normalized
        .into_iter()
        .map(|(canonical, _)| {
            let logical = logical_scan_root(project_root, &canonical)?;
            Ok((canonical, logical))
        })
        .collect::<Result<Vec<_>>>()?;
    candidates.sort_unstable_by(|left, right| compare_portable_paths(&left.1, &right.1));

    let mut effective: Vec<(PathBuf, String)> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if effective.iter().any(|(ancestor, _)| {
            strip_platform_prefix(ancestor.as_path(), candidate.0.as_path()).is_ok()
        }) {
            continue;
        }
        effective.push(candidate);
    }
    Ok(effective)
}

fn logical_scan_root(project_root: &Path, scan_root: &Path) -> Result<String> {
    let relative = strip_platform_prefix(project_root, scan_root).map_err(|_| {
        anyhow!(
            "normalized scan root escaped the project: {}",
            scan_root.display()
        )
    })?;
    let mut logical = String::new();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(anyhow!(
                "normalized scan root contains a non-relative component: {}",
                scan_root.display()
            ));
        };
        let name = name.to_str().ok_or_else(|| {
            anyhow!(
                "normalized scan root must be valid UTF-8: {}",
                scan_root.display()
            )
        })?;
        if !logical.is_empty() {
            logical.push('/');
        }
        logical.push_str(name);
    }
    Ok(logical)
}

fn platform_paths_equal(left: &Path, right: &Path) -> bool {
    strip_platform_prefix(left, right).is_ok_and(|relative| relative.as_os_str().is_empty())
        && strip_platform_prefix(right, left).is_ok_and(|relative| relative.as_os_str().is_empty())
}

fn platform_paths_overlap(left: &Path, right: &Path) -> bool {
    strip_platform_prefix(left, right).is_ok() || strip_platform_prefix(right, left).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    fn project_with_private_index_namespace(
        relative_namespace: &str,
    ) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temporary = crate::secure_test_tempdir();
        let project = temporary.path().join("project");
        let bootstrap = temporary.path().join("bootstrap");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&bootstrap).unwrap();
        let namespace = project.join(relative_namespace);
        IndexPaths::for_project(bootstrap, Some(namespace.clone()), None).unwrap();
        (temporary, project, namespace)
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn default_index_root_is_independent_of_library_presence() {
        let project = tempfile::tempdir().unwrap();
        for directory in ["Assets", "Packages", "ProjectSettings"] {
            std::fs::create_dir_all(project.path().join(directory)).unwrap();
        }

        let without_library =
            IndexPaths::for_project(project.path().to_path_buf(), None, None).unwrap();
        let index_root = without_library.index_root().to_path_buf();
        std::fs::create_dir(project.path().join("Library")).unwrap();
        let with_library =
            IndexPaths::for_project(project.path().to_path_buf(), None, None).unwrap();

        assert_eq!(without_library.scan_roots().len(), 3);
        assert_eq!(index_root, with_library.index_root());
        assert!(!index_root.starts_with(project.path()));
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn explicit_index_base_derives_distinct_project_bound_children() {
        let temporary = crate::secure_test_tempdir();
        let project_a = temporary.path().join("project-a");
        let project_b = temporary.path().join("project-b");
        let index_base = temporary.path().join("indices");
        std::fs::create_dir(&project_a).unwrap();
        std::fs::create_dir(&project_b).unwrap();

        let paths_a = IndexPaths::for_project(project_a, Some(index_base.clone()), None).unwrap();
        let paths_b = IndexPaths::for_project(project_b, Some(index_base.clone()), None).unwrap();

        assert_eq!(paths_a.index_root().parent(), Some(index_base.as_path()));
        assert_eq!(paths_b.index_root().parent(), Some(index_base.as_path()));
        assert_ne!(paths_a.index_root(), paths_b.index_root());
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn explicit_index_base_must_be_absolute() {
        let project = tempfile::tempdir().unwrap();

        let error = IndexPaths::for_project(
            project.path().to_path_buf(),
            Some(PathBuf::from("relative-index")),
            None,
        )
        .unwrap_err();

        assert!(error.to_string().contains("must be absolute"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn an_index_namespace_ancestor_is_not_a_project_source_exclusion() {
        let temporary = crate::secure_test_tempdir();
        let namespace = temporary.path().join("indices-and-projects");
        let bootstrap = temporary.path().join("bootstrap");
        std::fs::create_dir(&bootstrap).unwrap();
        IndexPaths::for_project(bootstrap, Some(namespace.clone()), None).unwrap();
        let project = namespace.join("project");
        std::fs::create_dir_all(project.join("Assets")).unwrap();

        let paths = IndexPaths::for_project(project, Some(namespace.clone()), None).unwrap();

        assert_eq!(paths.index_namespace_root(), namespace);
        assert_eq!(paths.index_namespace_exclusion(), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn scan_root_cannot_equal_the_private_index_namespace() {
        let (_temporary, project, namespace) = project_with_private_index_namespace("Assets");

        let error = IndexPaths::for_project(project, Some(namespace), None).unwrap_err();

        assert!(error.to_string().contains("private index namespace"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn scan_root_cannot_be_nested_below_the_private_index_namespace() {
        let (_temporary, project, namespace) = project_with_private_index_namespace("PrivateIndex");
        let scan_root = namespace.join("SourceLikeSubtree");
        std::fs::create_dir(&scan_root).unwrap();

        let error =
            IndexPaths::for_project(project, Some(namespace), Some(vec![scan_root])).unwrap_err();

        assert!(error.to_string().contains("private index namespace"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn project_moved_onto_its_derived_index_root_is_rejected() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = crate::secure_test_tempdir();
        let project = temporary.path().join("project");
        let index_base = temporary.path().join("indices");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(project.join("Assets")).unwrap();
        std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o700)).unwrap();

        let paths =
            IndexPaths::for_project(project.clone(), Some(index_base.clone()), None).unwrap();
        let derived_index_root = paths.index_root().to_path_buf();
        drop(paths);

        std::fs::rename(&derived_index_root, index_base.join("retired-index")).unwrap();
        std::fs::rename(&project, &derived_index_root).unwrap();

        let error =
            IndexPaths::for_project(derived_index_root, Some(index_base), None).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("project-bound private index root cannot equal the project root")
        );
    }

    #[cfg(windows)]
    #[test]
    fn scan_root_namespace_overlap_uses_windows_case_equivalence() {
        let (_temporary, project, namespace) = project_with_private_index_namespace("Assets");
        let case_alias = PathBuf::from(namespace.to_string_lossy().to_uppercase());

        let error =
            IndexPaths::for_project(project, Some(namespace), Some(vec![case_alias])).unwrap_err();

        assert!(error.to_string().contains("private index namespace"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn case_insensitive_apfs_aliases_publish_physical_scan_coordinates() {
        let temporary = crate::secure_test_tempdir();
        let project = temporary.path().join("Project");
        let bootstrap = temporary.path().join("Bootstrap");
        std::fs::create_dir_all(project.join("Assets")).unwrap();
        std::fs::create_dir(&bootstrap).unwrap();
        let namespace = project.join("Assets").join("SearchIndex");
        IndexPaths::for_project(bootstrap, Some(namespace.clone()), None).unwrap();

        let project_alias = temporary.path().join("project");
        if !project_alias.exists() {
            return;
        }
        let namespace_alias = project_alias.join("assets").join("searchindex");
        let paths = IndexPaths::for_project(project_alias, Some(namespace_alias), None).unwrap();
        let canonical_project = std::fs::canonicalize(&project).unwrap();
        let canonical_namespace = std::fs::canonicalize(&namespace).unwrap();

        assert_eq!(paths.project_root(), canonical_project);
        assert_eq!(
            paths.index_namespace_exclusion(),
            Some(canonical_namespace.as_path())
        );
        assert_eq!(paths.scan_roots(), &[canonical_project.join("Assets")]);
    }

    #[test]
    fn insecure_explicit_index_root_is_rejected() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("Assets")).unwrap();
        let override_root = project.path().join("index");
        std::fs::create_dir(&override_root).unwrap();

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(&override_root, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        assert!(
            IndexPaths::for_project(project.path().to_path_buf(), Some(override_root), None,)
                .is_err()
        );
    }

    #[test]
    fn scan_roots_cannot_escape_the_project() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("Assets")).unwrap();

        let error = IndexPaths::for_project(
            project.path().to_path_buf(),
            None,
            Some(vec![outside.path().to_path_buf()]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("inside project root"));
    }

    #[cfg(unix)]
    #[test]
    fn configured_scan_root_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let linked_root = project.path().join("Assets");
        symlink(outside.path(), &linked_root).unwrap();

        let error =
            IndexPaths::for_project(project.path().to_path_buf(), None, Some(vec![linked_root]))
                .unwrap_err();

        assert!(error.to_string().contains("without following links"));
    }

    #[cfg(unix)]
    #[test]
    fn absolute_scan_root_accepts_an_ancestor_alias_without_following_the_leaf() {
        use std::os::unix::fs::symlink;

        let temporary = crate::secure_test_tempdir();
        let physical_parent = temporary.path().join("physical");
        let alias = temporary.path().join("alias");
        let project = physical_parent.join("project");
        std::fs::create_dir_all(project.join("Assets")).unwrap();
        symlink(&physical_parent, &alias).unwrap();

        let alias_project = alias.join("project");
        let paths = IndexPaths::for_project(
            alias_project.clone(),
            None,
            Some(vec![alias_project.join("Assets")]),
        )
        .unwrap();

        assert_eq!(paths.scan_roots(), &[project.join("Assets")]);
    }

    #[cfg(windows)]
    #[test]
    fn configured_scan_root_rejects_directory_junctions() {
        use std::process::Command;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let linked_root = project.path().join("Assets");
        let output = Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&linked_root)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to create junction: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let error =
            IndexPaths::for_project(project.path().to_path_buf(), None, Some(vec![linked_root]))
                .unwrap_err();

        assert!(error.to_string().contains("without following links"));
    }

    #[cfg(unix)]
    #[test]
    fn configured_scan_root_rejects_non_portable_components() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path().join("Foo:Bar");
        std::fs::create_dir(&root).unwrap();

        let error = IndexPaths::for_project(project.path().to_path_buf(), None, Some(vec![root]))
            .unwrap_err();

        assert!(error.to_string().contains("non-portable component"));
    }

    #[cfg(windows)]
    #[test]
    fn configured_scan_roots_deduplicate_windows_case_aliases() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("Assets")).unwrap();

        let aliases = IndexPaths::for_project(
            project.path().to_path_buf(),
            None,
            Some(vec![PathBuf::from("Assets"), PathBuf::from("ASSETS")]),
        )
        .unwrap();
        let exact = IndexPaths::for_project(
            project.path().to_path_buf(),
            None,
            Some(vec![PathBuf::from("Assets")]),
        )
        .unwrap();

        assert_eq!(aliases.scan_roots(), exact.scan_roots());
        assert_eq!(
            aliases
                .logical_configuration_digest(SearchIndexOptions::default())
                .unwrap(),
            exact
                .logical_configuration_digest(SearchIndexOptions::default())
                .unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn absolute_scan_root_accepts_windows_case_aliases_of_the_project_path() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("Assets")).unwrap();
        let case_alias = PathBuf::from(
            project
                .path()
                .join("Assets")
                .to_string_lossy()
                .to_uppercase(),
        );

        let paths =
            IndexPaths::for_project(project.path().to_path_buf(), None, Some(vec![case_alias]))
                .unwrap();

        assert_eq!(paths.scan_roots().len(), 1);
    }

    #[test]
    fn scan_root_count_cannot_exceed_the_status_contract() {
        let project = tempfile::tempdir().unwrap();
        let mut roots = Vec::new();
        for ordinal in 0..=MAX_STATUS_SCAN_ROOTS {
            let root = project.path().join(format!("root-{ordinal}"));
            std::fs::create_dir_all(&root).unwrap();
            roots.push(root);
        }

        let error =
            IndexPaths::for_project(project.path().to_path_buf(), None, Some(roots)).unwrap_err();
        assert!(error.to_string().contains("scan root count"));
    }

    #[test]
    fn logical_configuration_binds_effective_scan_roots_but_not_retention() {
        let temporary = crate::secure_test_tempdir();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(project.join("Assets")).unwrap();
        std::fs::create_dir_all(project.join("Packages")).unwrap();
        let index = temporary.path().join("index");
        let assets = IndexPaths::for_project(
            project.clone(),
            Some(index.clone()),
            Some(vec![PathBuf::from("Assets")]),
        )
        .unwrap();
        let assets_and_packages = IndexPaths::for_project(
            project,
            Some(index),
            Some(vec![PathBuf::from("Packages"), PathBuf::from("Assets")]),
        )
        .unwrap();
        let options = SearchIndexOptions::default();
        let mut different_retention = options;
        different_retention.retain_previous_generations = 0;

        assert_eq!(
            assets.logical_configuration_digest(options).unwrap(),
            assets
                .logical_configuration_digest(different_retention)
                .unwrap()
        );
        assert_ne!(
            assets.logical_configuration_digest(options).unwrap(),
            assets_and_packages
                .logical_configuration_digest(options)
                .unwrap()
        );
        assert_eq!(
            assets_and_packages
                .logical_scan_roots
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["Assets", "Packages"]
        );
    }

    #[test]
    fn ancestor_scan_root_collapses_descendants_in_runtime_and_identity() {
        let temporary = crate::secure_test_tempdir();
        let project = temporary.path().join("project");
        std::fs::create_dir_all(project.join("Assets")).unwrap();
        let paths = IndexPaths::for_project(
            project.clone(),
            Some(temporary.path().join("index")),
            Some(vec![project.clone(), project.join("Assets")]),
        )
        .unwrap();

        assert_eq!(
            paths.scan_roots(),
            &[std::fs::canonicalize(project).unwrap()]
        );
        assert_eq!(
            paths
                .logical_scan_roots
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [""]
        );
    }

    #[test]
    fn options_reject_a_retention_threshold_above_the_hard_source_limit() {
        let options = SearchIndexOptions {
            max_source_bytes: 4,
            max_retained_source_bytes: 5,
            ..SearchIndexOptions::default()
        };

        assert!(options.validate().is_err());
    }

    #[test]
    fn options_accept_scan_and_ignore_limits_at_the_hard_bounds() {
        let options = SearchIndexOptions {
            scan_limits: ScanTraversalLimits {
                max_entries: SCAN_ENTRIES_HARD_MAX,
                max_path_bytes: SCAN_PATH_BYTES_HARD_MAX,
                max_depth: SCAN_DEPTH_HARD_MAX,
                max_directories: SCAN_DIRECTORIES_HARD_MAX,
                max_files: SCAN_FILES_HARD_MAX,
                max_diagnostics: SCAN_DIAGNOSTICS_HARD_MAX,
                max_policy_matches: SCAN_POLICY_MATCHES_HARD_MAX,
            },
            ignore_limits: SearchIgnoreV1Limits {
                max_file_bytes: SEARCH_IGNORE_FILE_BYTES_HARD_MAX,
                max_line_bytes: SEARCH_IGNORE_LINE_BYTES_HARD_MAX,
                max_rules: SEARCH_IGNORE_RULES_HARD_MAX,
                max_parser_work: SEARCH_IGNORE_PARSER_WORK_HARD_MAX,
                max_automaton_bytes: SEARCH_IGNORE_AUTOMATON_BYTES_HARD_MAX,
                max_compilation_bytes: SEARCH_IGNORE_COMPILATION_BYTES_HARD_MAX,
            },
            ..SearchIndexOptions::default()
        };

        assert!(options.validate().is_ok());
    }

    #[test]
    fn options_reject_scan_limits_above_the_hard_bounds() {
        let defaults = SearchIndexOptions::default();
        let cases = [
            SearchIndexOptions {
                scan_limits: ScanTraversalLimits {
                    max_entries: SCAN_ENTRIES_HARD_MAX + 1,
                    ..defaults.scan_limits
                },
                ..defaults
            },
            SearchIndexOptions {
                scan_limits: ScanTraversalLimits {
                    max_path_bytes: SCAN_PATH_BYTES_HARD_MAX + 1,
                    ..defaults.scan_limits
                },
                ..defaults
            },
            SearchIndexOptions {
                scan_limits: ScanTraversalLimits {
                    max_depth: SCAN_DEPTH_HARD_MAX + 1,
                    ..defaults.scan_limits
                },
                ..defaults
            },
            SearchIndexOptions {
                scan_limits: ScanTraversalLimits {
                    max_directories: SCAN_DIRECTORIES_HARD_MAX + 1,
                    ..defaults.scan_limits
                },
                ..defaults
            },
            SearchIndexOptions {
                scan_limits: ScanTraversalLimits {
                    max_files: SCAN_FILES_HARD_MAX + 1,
                    ..defaults.scan_limits
                },
                ..defaults
            },
            SearchIndexOptions {
                scan_limits: ScanTraversalLimits {
                    max_diagnostics: SCAN_DIAGNOSTICS_HARD_MAX + 1,
                    ..defaults.scan_limits
                },
                ..defaults
            },
            SearchIndexOptions {
                scan_limits: ScanTraversalLimits {
                    max_policy_matches: SCAN_POLICY_MATCHES_HARD_MAX + 1,
                    ..defaults.scan_limits
                },
                ..defaults
            },
        ];

        assert!(cases.into_iter().all(|options| options.validate().is_err()));
    }

    #[test]
    fn options_reject_ignore_limits_above_the_hard_bounds() {
        let defaults = SearchIndexOptions::default();
        let cases = [
            SearchIndexOptions {
                ignore_limits: SearchIgnoreV1Limits {
                    max_file_bytes: SEARCH_IGNORE_FILE_BYTES_HARD_MAX + 1,
                    ..defaults.ignore_limits
                },
                ..defaults
            },
            SearchIndexOptions {
                ignore_limits: SearchIgnoreV1Limits {
                    max_line_bytes: SEARCH_IGNORE_LINE_BYTES_HARD_MAX + 1,
                    ..defaults.ignore_limits
                },
                ..defaults
            },
            SearchIndexOptions {
                ignore_limits: SearchIgnoreV1Limits {
                    max_rules: SEARCH_IGNORE_RULES_HARD_MAX + 1,
                    ..defaults.ignore_limits
                },
                ..defaults
            },
            SearchIndexOptions {
                ignore_limits: SearchIgnoreV1Limits {
                    max_parser_work: SEARCH_IGNORE_PARSER_WORK_HARD_MAX + 1,
                    ..defaults.ignore_limits
                },
                ..defaults
            },
            SearchIndexOptions {
                ignore_limits: SearchIgnoreV1Limits {
                    max_automaton_bytes: SEARCH_IGNORE_AUTOMATON_BYTES_HARD_MAX + 1,
                    ..defaults.ignore_limits
                },
                ..defaults
            },
            SearchIndexOptions {
                ignore_limits: SearchIgnoreV1Limits {
                    max_compilation_bytes: SEARCH_IGNORE_COMPILATION_BYTES_HARD_MAX + 1,
                    ..defaults.ignore_limits
                },
                ..defaults
            },
        ];

        assert!(cases.into_iter().all(|options| options.validate().is_err()));
    }
}
