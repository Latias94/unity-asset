use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use unity_asset_core::DigestV1;
use unity_asset_search_protocol::{MAX_STATUS_SCAN_ROOTS, PortablePath, StatusResponse};

use crate::scan::ScanReadLimits;

pub(crate) const PROJECT_ROOT_IGNORE_FILE_BYTES_HARD_MAX: u64 = 1024 * 1024;
pub(crate) const PROJECT_ROOT_IGNORE_LINE_BYTES_HARD_MAX: usize = 16 * 1024;
pub(crate) const PROJECT_ROOT_IGNORE_PATTERNS_HARD_MAX: usize = 4 * 1024;
pub(crate) const PROJECT_ROOT_IGNORE_PARSER_WORK_HARD_MAX: u64 =
    2 * 3 * PROJECT_ROOT_IGNORE_FILE_BYTES_HARD_MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPaths {
    project_root: PathBuf,
    index_root: PathBuf,
    scan_roots: Vec<PathBuf>,
}

impl IndexPaths {
    pub fn for_project(
        project_root: PathBuf,
        index_root: Option<PathBuf>,
        scan_roots: Option<Vec<PathBuf>>,
    ) -> Result<Self> {
        let project_root = project_root
            .canonicalize()
            .with_context(|| format!("project root does not exist: {}", project_root.display()))?;
        ensure!(project_root.is_dir(), "project root is not a directory");

        let index_root = index_root.unwrap_or_else(|| default_index_root(&project_root));
        let index_root = if index_root.is_absolute() {
            index_root
        } else {
            project_root.join(index_root)
        };
        let index_root = std::path::absolute(&index_root)
            .with_context(|| format!("resolve search generation root: {}", index_root.display()))?;

        let scan_roots = match scan_roots {
            Some(roots) if !roots.is_empty() => roots,
            _ => default_scan_roots(&project_root),
        };
        let scan_roots = normalize_scan_roots(&project_root, scan_roots)?;
        validate_status_path_contract(&project_root, &index_root, &scan_roots)?;

        Ok(Self {
            project_root,
            index_root,
            scan_roots,
        })
    }

    #[must_use]
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    #[must_use]
    pub fn index_root(&self) -> &Path {
        &self.index_root
    }

    #[must_use]
    pub fn scan_roots(&self) -> &[PathBuf] {
        &self.scan_roots
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchIndexOptions {
    pub index_bundle_container_entries: bool,
    pub max_bundle_container_entries_per_bundle: usize,
    pub max_references_per_asset: usize,
    /// Apply `.ignore` and `.unity-asset-search-ignore` from the project root.
    ///
    /// Nested ignore files are deliberately unsupported so every rule source can be opened,
    /// bounded, and charged to the caller's scan budget before directory traversal starts. Rules
    /// follow standard gitignore matching semantics. `.unity-asset-search-ignore` has precedence
    /// over `.ignore`.
    pub respect_project_root_ignore_files: bool,
    /// Also apply `.gitignore` from the project root.
    ///
    /// This has no effect when `respect_project_root_ignore_files` is disabled. Its rules have
    /// lower precedence than `.ignore` and `.unity-asset-search-ignore`.
    pub respect_project_root_gitignore: bool,
    /// Maximum encoded bytes accepted from each configured project-root ignore file.
    pub max_project_root_ignore_file_bytes: u64,
    /// Maximum encoded bytes accepted from one project-root ignore rule line.
    pub max_project_root_ignore_line_bytes: usize,
    /// Maximum non-comment rules accepted across all project-root ignore files.
    pub max_project_root_ignore_patterns: usize,
    /// Maximum parser work, measured as encoded bytes inspected across validation and build passes.
    pub max_project_root_ignore_parser_work: u64,
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
        ensure!(
            self.max_project_root_ignore_file_bytes > 0
                && self.max_project_root_ignore_file_bytes
                    <= PROJECT_ROOT_IGNORE_FILE_BYTES_HARD_MAX,
            "max_project_root_ignore_file_bytes must be between 1 and \
             {PROJECT_ROOT_IGNORE_FILE_BYTES_HARD_MAX}"
        );
        ensure!(
            self.max_project_root_ignore_line_bytes > 0
                && self.max_project_root_ignore_line_bytes
                    <= PROJECT_ROOT_IGNORE_LINE_BYTES_HARD_MAX,
            "max_project_root_ignore_line_bytes must be between 1 and \
             {PROJECT_ROOT_IGNORE_LINE_BYTES_HARD_MAX}"
        );
        ensure!(
            u64::try_from(self.max_project_root_ignore_line_bytes)
                .is_ok_and(|line| line <= self.max_project_root_ignore_file_bytes),
            "max_project_root_ignore_line_bytes cannot exceed \
             max_project_root_ignore_file_bytes"
        );
        ensure!(
            self.max_project_root_ignore_patterns > 0
                && self.max_project_root_ignore_patterns <= PROJECT_ROOT_IGNORE_PATTERNS_HARD_MAX,
            "max_project_root_ignore_patterns must be between 1 and \
             {PROJECT_ROOT_IGNORE_PATTERNS_HARD_MAX}"
        );
        ensure!(
            self.max_project_root_ignore_parser_work > 0
                && self.max_project_root_ignore_parser_work
                    <= PROJECT_ROOT_IGNORE_PARSER_WORK_HARD_MAX,
            "max_project_root_ignore_parser_work must be between 1 and \
             {PROJECT_ROOT_IGNORE_PARSER_WORK_HARD_MAX}"
        );
        ensure!(
            usize::try_from(self.max_project_root_ignore_file_bytes).is_ok(),
            "max_project_root_ignore_file_bytes is not addressable on this platform"
        );
        ensure!(
            self.retain_previous_generations <= 32,
            "retain_previous_generations cannot exceed 32"
        );
        Ok(self)
    }

    pub(crate) fn logical_digest(self) -> Result<DigestV1> {
        serde_json::to_vec(&self)
            .map(|bytes| DigestV1::hash_bytes(&bytes))
            .map_err(|error| anyhow!("serialize search options: {error}"))
    }

    pub(crate) const fn scan_limits(self) -> ScanReadLimits {
        ScanReadLimits {
            max_asset_bytes: self.max_source_bytes,
            max_retained_asset_bytes: self.max_retained_source_bytes,
            max_meta_bytes: self.max_metadata_bytes,
        }
    }
}

impl Default for SearchIndexOptions {
    fn default() -> Self {
        Self {
            index_bundle_container_entries: false,
            max_bundle_container_entries_per_bundle: 50_000,
            max_references_per_asset: 100_000,
            respect_project_root_ignore_files: true,
            respect_project_root_gitignore: true,
            max_project_root_ignore_file_bytes: 256 * 1024,
            max_project_root_ignore_line_bytes: 8 * 1024,
            max_project_root_ignore_patterns: 1024,
            max_project_root_ignore_parser_work: 2 * 3 * 256 * 1024,
            max_source_bytes: 2 * 1024 * 1024 * 1024,
            max_retained_source_bytes: 64 * 1024 * 1024,
            max_metadata_bytes: 8 * 1024 * 1024,
            retain_previous_generations: 2,
        }
    }
}

fn default_index_root(project_root: &Path) -> PathBuf {
    let library = project_root.join("Library");
    if library.is_dir() {
        library.join("unity-asset-search")
    } else {
        project_root.join(".unity-asset-search")
    }
}

fn default_scan_roots(project_root: &Path) -> Vec<PathBuf> {
    let assets = project_root.join("Assets");
    if !assets.is_dir() {
        return vec![project_root.to_path_buf()];
    }

    let roots = ["Assets", "Packages", "ProjectSettings"]
        .into_iter()
        .map(|directory| project_root.join(directory))
        .filter(|candidate| candidate.is_dir())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        vec![project_root.to_path_buf()]
    } else {
        roots
    }
}

fn normalize_scan_roots(project_root: &Path, roots: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let mut normalized = Vec::new();
    for root in roots {
        let root = if root.is_absolute() {
            root
        } else {
            project_root.join(root)
        };
        let root = root
            .canonicalize()
            .with_context(|| format!("scan root does not exist: {}", root.display()))?;
        if !root.starts_with(project_root) {
            return Err(anyhow!(
                "scan root must be inside project root: {}",
                root.display()
            ));
        }
        if !root.is_dir() {
            return Err(anyhow!("scan root is not a directory: {}", root.display()));
        }
        normalized.push(root);
    }
    normalized.sort();
    normalized.dedup();
    ensure!(
        normalized.len() <= MAX_STATUS_SCAN_ROOTS,
        "scan root count {} exceeds the protocol maximum of {MAX_STATUS_SCAN_ROOTS}",
        normalized.len()
    );
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roots_cover_existing_unity_directories() {
        let project = tempfile::tempdir().unwrap();
        for directory in ["Assets", "Packages", "ProjectSettings"] {
            std::fs::create_dir_all(project.path().join(directory)).unwrap();
        }

        let paths = IndexPaths::for_project(project.path().to_path_buf(), None, None).unwrap();

        assert_eq!(paths.scan_roots().len(), 3);
        assert!(paths.index_root().ends_with(".unity-asset-search"));
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
    fn options_reject_a_retention_threshold_above_the_hard_source_limit() {
        let options = SearchIndexOptions {
            max_source_bytes: 4,
            max_retained_source_bytes: 5,
            ..SearchIndexOptions::default()
        };

        assert!(options.validate().is_err());
    }

    #[test]
    fn options_accept_project_root_ignore_limits_at_the_hard_bounds() {
        let options = SearchIndexOptions {
            max_project_root_ignore_file_bytes: PROJECT_ROOT_IGNORE_FILE_BYTES_HARD_MAX,
            max_project_root_ignore_line_bytes: PROJECT_ROOT_IGNORE_LINE_BYTES_HARD_MAX,
            max_project_root_ignore_patterns: PROJECT_ROOT_IGNORE_PATTERNS_HARD_MAX,
            max_project_root_ignore_parser_work: PROJECT_ROOT_IGNORE_PARSER_WORK_HARD_MAX,
            ..SearchIndexOptions::default()
        };

        assert!(options.validate().is_ok());
    }

    #[test]
    fn options_reject_project_root_ignore_limits_above_the_hard_bounds() {
        let cases = [
            SearchIndexOptions {
                max_project_root_ignore_file_bytes: PROJECT_ROOT_IGNORE_FILE_BYTES_HARD_MAX + 1,
                ..SearchIndexOptions::default()
            },
            SearchIndexOptions {
                max_project_root_ignore_line_bytes: PROJECT_ROOT_IGNORE_LINE_BYTES_HARD_MAX + 1,
                ..SearchIndexOptions::default()
            },
            SearchIndexOptions {
                max_project_root_ignore_patterns: PROJECT_ROOT_IGNORE_PATTERNS_HARD_MAX + 1,
                ..SearchIndexOptions::default()
            },
            SearchIndexOptions {
                max_project_root_ignore_parser_work: PROJECT_ROOT_IGNORE_PARSER_WORK_HARD_MAX + 1,
                ..SearchIndexOptions::default()
            },
        ];

        assert!(cases.into_iter().all(|options| options.validate().is_err()));
    }
}
