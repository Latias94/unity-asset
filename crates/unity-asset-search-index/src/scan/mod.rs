mod candidate;
mod diagnostic;
mod ledger;
mod policy;
mod scanner;

pub(crate) use candidate::{FileHint, ProjectSourcePath, ReadSource, SourceHints};
pub(crate) use diagnostic::{PathRejection, ScanDiagnostic, SourcePart};
pub(crate) use scanner::{
    ProjectScanner, ScanError, ScanIntent, ScanMetrics, ScanMode, ScanReadLimits, ScanValidation,
};
