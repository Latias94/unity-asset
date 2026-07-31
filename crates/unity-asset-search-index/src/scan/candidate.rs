use std::path::PathBuf;

use unity_asset_core::{BudgetedSourceBytes, DigestV1};
use unity_asset_search_core::SearchKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanCandidate {
    pub(crate) rel_path: String,
    pub(crate) name: String,
    pub(crate) kind: SearchKind,
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
