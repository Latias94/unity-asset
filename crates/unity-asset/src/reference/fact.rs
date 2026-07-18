use std::fmt;

use unity_asset_core::{
    Diagnostic, FieldPath, ObjectAddress, RevisionedObjectHandle, SourceLocator,
};

/// On-disk format that produced a reference occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReferenceFormat {
    Binary,
    Yaml,
}

impl fmt::Display for ReferenceFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Binary => "binary",
            Self::Yaml => "YAML",
        })
    }
}

/// External-table identity retained from a SerializedFile reference.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BinaryExternalReference {
    index: u32,
    guid: Option<[u8; 16]>,
    type_id: i32,
    path: String,
}

impl BinaryExternalReference {
    pub(crate) fn new(index: u32, guid: [u8; 16], type_id: i32, path: String) -> Self {
        Self {
            index,
            guid: (guid != [0; 16]).then_some(guid),
            type_id,
            path,
        }
    }

    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    #[must_use]
    pub const fn guid(&self) -> Option<[u8; 16]> {
        self.guid
    }

    #[must_use]
    pub const fn type_id(&self) -> i32 {
        self.type_id
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// GUID spelling retained from a YAML reference.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReferenceGuid {
    Parsed([u8; 16]),
    Invalid(String),
}

impl ReferenceGuid {
    #[must_use]
    pub const fn parsed(&self) -> Option<[u8; 16]> {
        match self {
            Self::Parsed(guid) => Some(*guid),
            Self::Invalid(_) => None,
        }
    }

    #[must_use]
    pub fn invalid_spelling(&self) -> Option<&str> {
        match self {
            Self::Parsed(_) => None,
            Self::Invalid(value) => Some(value),
        }
    }
}

/// Format-faithful target before workspace resolution.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RawReferenceTarget {
    Binary {
        file_id: i32,
        path_id: i64,
        external: Option<BinaryExternalReference>,
    },
    Yaml {
        file_id: Option<i64>,
        guid: Option<ReferenceGuid>,
        type_id: Option<i64>,
    },
}

impl RawReferenceTarget {
    #[must_use]
    pub const fn format(&self) -> ReferenceFormat {
        match self {
            Self::Binary { .. } => ReferenceFormat::Binary,
            Self::Yaml { .. } => ReferenceFormat::Yaml,
        }
    }
}

/// Resolution of one occurrence against an exact workspace revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceResolution {
    Null,
    Resolved(RevisionedObjectHandle),
    Unloaded { source: Option<SourceLocator> },
    Missing { target: Option<ObjectAddress> },
    Ambiguous { candidates: Box<[ObjectAddress]> },
    Invalid { diagnostic: Diagnostic },
}

impl ReferenceResolution {
    #[must_use]
    pub const fn resolved(&self) -> Option<&RevisionedObjectHandle> {
        match self {
            Self::Resolved(target) => Some(target),
            Self::Null
            | Self::Unloaded { .. }
            | Self::Missing { .. }
            | Self::Ambiguous { .. }
            | Self::Invalid { .. } => None,
        }
    }
}

/// One reference occurrence bound to its source object and workspace revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceFact {
    source: RevisionedObjectHandle,
    field_path: FieldPath,
    raw_target: RawReferenceTarget,
    resolution: ReferenceResolution,
    diagnostics: Box<[Diagnostic]>,
}

impl ReferenceFact {
    pub(crate) fn new(
        source: RevisionedObjectHandle,
        field_path: FieldPath,
        raw_target: RawReferenceTarget,
        resolution: ReferenceResolution,
        diagnostics: Box<[Diagnostic]>,
    ) -> Self {
        Self {
            source,
            field_path,
            raw_target,
            resolution,
            diagnostics,
        }
    }

    #[must_use]
    pub const fn source(&self) -> &RevisionedObjectHandle {
        &self.source
    }

    #[must_use]
    pub const fn field_path(&self) -> &FieldPath {
        &self.field_path
    }

    #[must_use]
    pub const fn raw_target(&self) -> &RawReferenceTarget {
        &self.raw_target
    }

    #[must_use]
    pub const fn resolution(&self) -> &ReferenceResolution {
        &self.resolution
    }

    #[must_use]
    pub const fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
}
