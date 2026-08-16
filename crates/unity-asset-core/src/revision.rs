use std::fmt;

use serde::{Deserialize, Serialize};

use crate::DigestV1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Yaml,
    SerializedFile,
    AssetBundle,
    WebFile,
    Archive,
    StreamedResource,
}

impl SourceKind {
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Yaml => "yaml",
            Self::SerializedFile => "serialized_file",
            Self::AssetBundle => "asset_bundle",
            Self::WebFile => "web_file",
            Self::Archive => "archive",
            Self::StreamedResource => "streamed_resource",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFingerprint {
    kind: SourceKind,
    digest: DigestV1,
}

impl SourceFingerprint {
    #[must_use]
    pub const fn new(kind: SourceKind, digest: DigestV1) -> Self {
        Self { kind, digest }
    }

    #[must_use]
    pub fn from_bytes(kind: SourceKind, bytes: &[u8]) -> Self {
        Self::new(kind, DigestV1::hash_bytes(bytes))
    }

    #[must_use]
    pub const fn digest(self) -> DigestV1 {
        self.digest
    }

    #[must_use]
    pub const fn kind(self) -> SourceKind {
        self.kind
    }
}

impl fmt::Display for SourceFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.kind.tag(), self.digest)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceRevision(DigestV1);

impl WorkspaceRevision {
    #[must_use]
    pub const fn new(digest: DigestV1) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn digest(self) -> DigestV1 {
        self.0
    }
}

impl fmt::Display for WorkspaceRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}
