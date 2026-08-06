use std::io;
use std::path::PathBuf;

use unity_asset_core::BudgetError;

use super::candidate::FileHint;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SourcePart {
    Asset,
    Meta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PathRejection {
    InvalidPath,
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
