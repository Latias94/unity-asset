use std::sync::Arc;

use unity_asset_binary::asset::{FileIdentifier, SerializedFile};
use unity_asset_binary::typetree::TypeTreeParseOptions;
use unity_asset_core::{
    AssetLoadBudget, ObjectAddress, ObjectId, SourceFingerprint, SourceId, SourceKind,
    SourceLocator, UnityClass, UnityDocument, WorkspaceId, WorkspaceRevision,
};
use unity_asset_write::artifact::PreparedArtifactSet;
use unity_asset_yaml::YamlDocument;

use super::{ReferenceGraphError, ReferenceStore};

pub(crate) mod sealed {
    pub trait Sealed {}
}

/// Closed input boundary for reference discovery over immutable workspace-like state.
pub(crate) trait ReferenceInput: sealed::Sealed {
    fn workspace_id(&self) -> WorkspaceId;

    fn revision(&self) -> WorkspaceRevision;

    fn object_source_count(&self) -> usize;

    fn object_sources(
        &self,
    ) -> impl Iterator<Item = Result<ReferenceSource<'_>, ReferenceGraphError>>;

    fn reference_store(&self) -> &ReferenceStore;

    fn typetree_options(&self) -> TypeTreeParseOptions;

    fn address_for_object(
        &self,
        object: &ObjectId,
        budget: &mut AssetLoadBudget,
    ) -> Result<ObjectAddress, ReferenceGraphError>;
}

pub(super) fn collect_object_sources<'source, I: ReferenceInput + ?Sized>(
    input: &'source I,
    mut sources: Vec<ReferenceSource<'source>>,
) -> Result<Vec<ReferenceSource<'source>>, ReferenceGraphError> {
    debug_assert!(sources.is_empty());
    let expected = input.object_source_count();
    for source in input.object_sources() {
        if sources.len() == expected {
            return Err(ReferenceGraphError::Invariant(
                "reference input exposed more object sources than declared",
            ));
        }
        sources.push(source?);
    }
    if sources.len() != expected {
        return Err(ReferenceGraphError::Invariant(
            "reference input exposed fewer object sources than declared",
        ));
    }
    Ok(sources)
}

/// Sparse semantic and wire overlay used while scanning a prepared source.
pub(crate) trait PreparedReferenceOverlay: std::fmt::Debug + Send + Sync {
    fn binary_replacement(&self, source: SourceId, path_id: i64) -> Option<&[u8]>;

    fn binary_external<'overlay>(
        &'overlay self,
        source: SourceId,
        file: &'overlay SerializedFile,
        index: usize,
    ) -> Option<&'overlay FileIdentifier>;

    fn yaml_class<'overlay>(
        &'overlay self,
        source: SourceId,
        document_index: usize,
        base: &'overlay UnityClass,
    ) -> &'overlay UnityClass;
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ReferenceSource<'source> {
    source: SourceId,
    fingerprint: SourceFingerprint,
    owner: ReferenceSourceOwner<'source>,
    locator: &'source SourceLocator,
    parent: Option<SourceId>,
    parse: ReferenceSourceParse<'source>,
}

impl<'source> ReferenceSource<'source> {
    pub(crate) fn serialized(
        source: SourceId,
        fingerprint: SourceFingerprint,
        owner: ReferenceSourceOwner<'source>,
        locator: &'source SourceLocator,
        parent: Option<SourceId>,
        file: &'source SerializedFile,
    ) -> Result<Self, ReferenceGraphError> {
        Self::checked(
            source,
            fingerprint,
            owner,
            locator,
            parent,
            ReferenceSourceParse::Serialized(file),
        )
    }

    pub(crate) fn yaml(
        source: SourceId,
        fingerprint: SourceFingerprint,
        owner: ReferenceSourceOwner<'source>,
        locator: &'source SourceLocator,
        parent: Option<SourceId>,
        document: &'source YamlDocument,
    ) -> Result<Self, ReferenceGraphError> {
        Self::checked(
            source,
            fingerprint,
            owner,
            locator,
            parent,
            ReferenceSourceParse::Yaml(document),
        )
    }

    pub(crate) fn prepared_serialized(
        source: SourceId,
        fingerprint: SourceFingerprint,
        owner: ReferenceSourceOwner<'source>,
        locator: &'source SourceLocator,
        parent: Option<SourceId>,
        file: &'source SerializedFile,
        overlay: &'source dyn PreparedReferenceOverlay,
    ) -> Result<Self, ReferenceGraphError> {
        Self::checked(
            source,
            fingerprint,
            owner,
            locator,
            parent,
            ReferenceSourceParse::PreparedSerialized {
                source,
                file,
                overlay,
            },
        )
    }

    pub(crate) fn prepared_yaml(
        source: SourceId,
        fingerprint: SourceFingerprint,
        owner: ReferenceSourceOwner<'source>,
        locator: &'source SourceLocator,
        parent: Option<SourceId>,
        document: &'source YamlDocument,
        overlay: &'source dyn PreparedReferenceOverlay,
    ) -> Result<Self, ReferenceGraphError> {
        Self::checked(
            source,
            fingerprint,
            owner,
            locator,
            parent,
            ReferenceSourceParse::PreparedYaml {
                source,
                document,
                overlay,
            },
        )
    }

    fn checked(
        source: SourceId,
        fingerprint: SourceFingerprint,
        owner: ReferenceSourceOwner<'source>,
        locator: &'source SourceLocator,
        parent: Option<SourceId>,
        parse: ReferenceSourceParse<'source>,
    ) -> Result<Self, ReferenceGraphError> {
        let parse_kind = parse.kind();
        let source_kind = source.kind();
        let fingerprint_kind = fingerprint.kind();
        if source_kind != parse_kind || fingerprint_kind != parse_kind {
            return Err(ReferenceGraphError::ReferenceSourceKindMismatch {
                source_id: source,
                source_kind,
                fingerprint_kind,
                parse_kind,
            });
        }
        Ok(Self {
            source,
            fingerprint,
            owner,
            locator,
            parent,
            parse,
        })
    }

    pub(crate) const fn source(self) -> SourceId {
        self.source
    }

    pub(crate) const fn fingerprint(self) -> SourceFingerprint {
        self.fingerprint
    }

    pub(crate) const fn owner(self) -> ReferenceSourceOwner<'source> {
        self.owner
    }

    pub(crate) const fn locator(self) -> &'source SourceLocator {
        self.locator
    }

    pub(crate) const fn parent(self) -> Option<SourceId> {
        self.parent
    }

    pub(crate) const fn parse(self) -> ReferenceSourceParse<'source> {
        self.parse
    }

    pub(crate) const fn yaml_document(self) -> Option<&'source YamlDocument> {
        match self.parse {
            ReferenceSourceParse::Serialized(_)
            | ReferenceSourceParse::PreparedSerialized { .. } => None,
            ReferenceSourceParse::Yaml(document)
            | ReferenceSourceParse::PreparedYaml { document, .. } => Some(document),
        }
    }

    pub(crate) const fn serialized_file(self) -> Option<&'source SerializedFile> {
        match self.parse {
            ReferenceSourceParse::Serialized(file)
            | ReferenceSourceParse::PreparedSerialized { file, .. } => Some(file),
            ReferenceSourceParse::Yaml(_) | ReferenceSourceParse::PreparedYaml { .. } => None,
        }
    }

    #[cfg(feature = "decode")]
    pub(crate) fn binary_external(self, index: usize) -> Option<&'source FileIdentifier> {
        match self.parse {
            ReferenceSourceParse::Serialized(file) => file.externals.get(index),
            ReferenceSourceParse::PreparedSerialized {
                source,
                file,
                overlay,
            } => overlay.binary_external(source, file, index),
            ReferenceSourceParse::Yaml(_) | ReferenceSourceParse::PreparedYaml { .. } => None,
        }
    }

    pub(crate) fn yaml_class(self, document_index: usize) -> Option<&'source UnityClass> {
        match self.parse {
            ReferenceSourceParse::Serialized(_)
            | ReferenceSourceParse::PreparedSerialized { .. } => None,
            ReferenceSourceParse::Yaml(document) => document.entries().get(document_index),
            ReferenceSourceParse::PreparedYaml {
                source,
                document,
                overlay,
            } => document
                .entries()
                .get(document_index)
                .map(|base| overlay.yaml_class(source, document_index, base)),
        }
    }
}

/// Borrowed lifetime owner for fingerprint-addressed reference facts.
///
/// The owner is deliberately independent from the source's byte access strategy. Prepared
/// artifacts can therefore retain segmented proof images without materializing a contiguous
/// backing allocation.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ReferenceSourceOwner<'owner> {
    Committed(&'owner Arc<[u8]>),
    Prepared(&'owner Arc<PreparedArtifactSet>),
}

impl<'owner> From<&'owner Arc<[u8]>> for ReferenceSourceOwner<'owner> {
    fn from(owner: &'owner Arc<[u8]>) -> Self {
        Self::Committed(owner)
    }
}

impl<'owner> From<&'owner Arc<PreparedArtifactSet>> for ReferenceSourceOwner<'owner> {
    fn from(owner: &'owner Arc<PreparedArtifactSet>) -> Self {
        Self::Prepared(owner)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum WeakReferenceSourceOwner {
    Committed(std::sync::Weak<[u8]>),
    Prepared(std::sync::Weak<PreparedArtifactSet>),
}

impl ReferenceSourceOwner<'_> {
    pub(crate) fn downgrade(self) -> WeakReferenceSourceOwner {
        match self {
            Self::Committed(owner) => WeakReferenceSourceOwner::Committed(Arc::downgrade(owner)),
            Self::Prepared(owner) => WeakReferenceSourceOwner::Prepared(Arc::downgrade(owner)),
        }
    }
}

impl WeakReferenceSourceOwner {
    pub(crate) fn is_live(&self) -> bool {
        match self {
            Self::Committed(owner) => owner.strong_count() != 0,
            Self::Prepared(owner) => owner.strong_count() != 0,
        }
    }

    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Committed(left), Self::Committed(right)) => std::sync::Weak::ptr_eq(left, right),
            (Self::Prepared(left), Self::Prepared(right)) => std::sync::Weak::ptr_eq(left, right),
            (Self::Committed(_), Self::Prepared(_)) | (Self::Prepared(_), Self::Committed(_)) => {
                false
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ReferenceSourceParse<'source> {
    Serialized(&'source SerializedFile),
    Yaml(&'source YamlDocument),
    PreparedSerialized {
        source: SourceId,
        file: &'source SerializedFile,
        overlay: &'source dyn PreparedReferenceOverlay,
    },
    PreparedYaml {
        source: SourceId,
        document: &'source YamlDocument,
        overlay: &'source dyn PreparedReferenceOverlay,
    },
}

impl ReferenceSourceParse<'_> {
    const fn kind(self) -> SourceKind {
        match self {
            Self::Serialized(_) | Self::PreparedSerialized { .. } => SourceKind::SerializedFile,
            Self::Yaml(_) | Self::PreparedYaml { .. } => SourceKind::Yaml,
        }
    }
}

#[cfg(test)]
mod tests {
    use unity_asset_core::{SourceKind, WorkspaceId};

    use super::*;

    fn source(kind: SourceKind) -> SourceId {
        SourceId::new(WorkspaceId::from_u128(1).unwrap(), kind, 1).unwrap()
    }

    #[test]
    fn yaml_source_rejects_a_non_yaml_source_identity() {
        let owner = Arc::<[u8]>::from([]);
        let locator = SourceLocator::path("source.prefab").unwrap();
        let document = YamlDocument::from_entries(Vec::new());
        let source = source(SourceKind::SerializedFile);
        let error = ReferenceSource::yaml(
            source,
            SourceFingerprint::from_bytes(SourceKind::Yaml, owner.as_ref()),
            (&owner).into(),
            &locator,
            None,
            &document,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ReferenceGraphError::ReferenceSourceKindMismatch {
                source_id: actual,
                source_kind: SourceKind::SerializedFile,
                fingerprint_kind: SourceKind::Yaml,
                parse_kind: SourceKind::Yaml,
            } if actual == source
        ));
    }

    #[test]
    fn yaml_source_rejects_a_non_yaml_fingerprint() {
        let owner = Arc::<[u8]>::from([]);
        let locator = SourceLocator::path("source.prefab").unwrap();
        let document = YamlDocument::from_entries(Vec::new());
        let source = source(SourceKind::Yaml);
        let error = ReferenceSource::yaml(
            source,
            SourceFingerprint::from_bytes(SourceKind::SerializedFile, owner.as_ref()),
            (&owner).into(),
            &locator,
            None,
            &document,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ReferenceGraphError::ReferenceSourceKindMismatch {
                source_id: actual,
                source_kind: SourceKind::Yaml,
                fingerprint_kind: SourceKind::SerializedFile,
                parse_kind: SourceKind::Yaml,
            } if actual == source
        ));
    }
}
