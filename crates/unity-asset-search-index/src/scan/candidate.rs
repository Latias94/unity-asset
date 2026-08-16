use std::path::PathBuf;

use unity_asset_core::{BudgetedSourceBytes, DigestV1};
use unity_asset_search_core::SearchKind;

use crate::source_coordinate::IndexedSourceCoordinate;
use crate::{ProjectPath, ProjectPathIdentity};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectSourcePath {
    identity: ProjectPathIdentity,
    relative_path: String,
}

impl ProjectSourcePath {
    pub(crate) fn from_project_path(path: &ProjectPath, relative_path: String) -> Self {
        debug_assert_eq!(
            path.as_relative_path(),
            std::path::Path::new(&relative_path)
        );
        Self {
            identity: path.identity(),
            relative_path,
        }
    }

    pub(crate) fn from_validated_parts(
        identity: ProjectPathIdentity,
        relative_path: String,
    ) -> Self {
        Self {
            identity,
            relative_path,
        }
    }

    #[must_use]
    pub(crate) const fn identity(&self) -> ProjectPathIdentity {
        self.identity
    }

    #[must_use]
    pub(crate) fn relative_path(&self) -> &str {
        &self.relative_path
    }

    #[must_use]
    pub(crate) fn relative_path_capacity(&self) -> usize {
        self.relative_path.capacity()
    }

    #[must_use]
    pub(crate) const fn coordinate(&self) -> IndexedSourceCoordinate {
        IndexedSourceCoordinate::project(self.identity)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanCandidate {
    path: ProjectSourcePath,
    pub(crate) name: String,
    pub(crate) kind: SearchKind,
}

impl ScanCandidate {
    pub(crate) fn new(path: ProjectSourcePath, name: String, kind: SearchKind) -> Self {
        Self { path, name, kind }
    }

    #[must_use]
    pub(crate) fn relative_path(&self) -> &str {
        self.path.relative_path()
    }

    #[must_use]
    pub(crate) fn relative_path_capacity(&self) -> usize {
        self.path.relative_path_capacity()
    }

    #[must_use]
    pub(crate) const fn coordinate(&self) -> IndexedSourceCoordinate {
        self.path.coordinate()
    }
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
    pub(crate) coordinate: IndexedSourceCoordinate,
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
