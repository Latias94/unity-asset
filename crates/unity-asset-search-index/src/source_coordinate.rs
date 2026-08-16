use serde::{Deserialize, Serialize};
use unity_asset_core::SourceId;

use crate::ProjectPathIdentity;

/// Stable authority for one indexed source, independent of its display spelling.
///
/// Files discovered below the project root use platform-aware path identity. Sources that only
/// exist in the logical workspace use their exact workspace identity instead; their aliases must
/// never inherit Windows filesystem comparison rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum IndexedSourceCoordinate {
    Project { path: ProjectPathIdentity },
    Workspace { source: SourceId },
}

impl IndexedSourceCoordinate {
    #[must_use]
    pub(crate) const fn project(path: ProjectPathIdentity) -> Self {
        Self::Project { path }
    }

    #[must_use]
    pub(crate) const fn workspace(source: SourceId) -> Self {
        Self::Workspace { source }
    }

    #[must_use]
    pub(crate) const fn project_path(self) -> Option<ProjectPathIdentity> {
        match self {
            Self::Project { path } => Some(path),
            Self::Workspace { .. } => None,
        }
    }
}
