//! Public API contract for the `unity-asset-search-index` package.

pub use unity_asset_search_index::{
    AssetLoadBudget, ChangeSet, DigestV1, FilesystemReindexIntent, FilesystemReindexScope,
    IndexPaths, ProjectPath, ProjectPathIdentity, ProjectPathSemantics, ProjectPathSet,
    ProjectPathSpace, ScanTraversalLimits, SearchIgnoreV1Limits, SearchIndex, SearchIndexOptions,
    SearchKind, SearchRequest, WorkspaceView, is_search_ignore_v1_file_name,
};
