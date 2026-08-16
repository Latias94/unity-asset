use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::bounded::BoundedVec;
use crate::{DigestV1, ObjectAddress, ObjectId, SourceId, WorkspaceId, WorkspaceRevision};

const MAX_CHANGE_SET_ITEMS: usize = 1_000_000;

/// Current wire version of the authoritative workspace change set.
pub const CHANGE_SET_VERSION: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransactionId(DigestV1);

impl TransactionId {
    #[must_use]
    pub const fn new(digest: DigestV1) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn digest(self) -> DigestV1 {
        self.0
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct IdentityRemap {
    from: ObjectAddress,
    to: ObjectAddress,
}

impl IdentityRemap {
    pub fn new(from: ObjectAddress, to: ObjectAddress) -> Result<Self, ChangeSetError> {
        if from == to {
            return Err(ChangeSetError::IdentityDidNotChange);
        }
        Ok(Self { from, to })
    }

    #[must_use]
    pub const fn from(&self) -> &ObjectAddress {
        &self.from
    }

    #[must_use]
    pub const fn to(&self) -> &ObjectAddress {
        &self.to
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityRemapWire {
    from: ObjectAddress,
    to: ObjectAddress,
}

impl Serialize for IdentityRemap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        IdentityRemapWire {
            from: self.from.clone(),
            to: self.to.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for IdentityRemap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = IdentityRemapWire::deserialize(deserializer)?;
        Self::new(wire.from, wire.to).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSet {
    transaction: TransactionId,
    workspace: WorkspaceId,
    from_revision: WorkspaceRevision,
    to_revision: WorkspaceRevision,
    changed_sources: Vec<SourceId>,
    changed_objects: Vec<ObjectId>,
    identity_remaps: Vec<IdentityRemap>,
}

#[derive(Serialize)]
struct ChangeSetRef<'a> {
    version: u8,
    transaction: TransactionId,
    workspace: WorkspaceId,
    from_revision: WorkspaceRevision,
    to_revision: WorkspaceRevision,
    changed_sources: &'a [SourceId],
    changed_objects: &'a [ObjectId],
    identity_remaps: &'a [IdentityRemap],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangeSetWire {
    version: u8,
    transaction: TransactionId,
    workspace: WorkspaceId,
    from_revision: WorkspaceRevision,
    to_revision: WorkspaceRevision,
    changed_sources: BoundedVec<SourceId, MAX_CHANGE_SET_ITEMS>,
    changed_objects: BoundedVec<ObjectId, MAX_CHANGE_SET_ITEMS>,
    identity_remaps: BoundedVec<IdentityRemap, MAX_CHANGE_SET_ITEMS>,
}

impl Serialize for ChangeSet {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ChangeSetRef {
            version: CHANGE_SET_VERSION,
            transaction: self.transaction,
            workspace: self.workspace,
            from_revision: self.from_revision,
            to_revision: self.to_revision,
            changed_sources: &self.changed_sources,
            changed_objects: &self.changed_objects,
            identity_remaps: &self.identity_remaps,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ChangeSet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ChangeSetWire::deserialize(deserializer)?;
        if wire.version != CHANGE_SET_VERSION {
            return Err(serde::de::Error::custom(
                ChangeSetError::UnsupportedVersion(wire.version),
            ));
        }
        Self::new(
            wire.transaction,
            wire.workspace,
            wire.from_revision,
            wire.to_revision,
            wire.changed_sources.into_vec(),
            wire.changed_objects.into_vec(),
            wire.identity_remaps.into_vec(),
        )
        .map_err(serde::de::Error::custom)
    }
}

impl ChangeSet {
    pub fn new(
        transaction: TransactionId,
        workspace: WorkspaceId,
        from_revision: WorkspaceRevision,
        to_revision: WorkspaceRevision,
        mut changed_sources: Vec<SourceId>,
        mut changed_objects: Vec<ObjectId>,
        mut identity_remaps: Vec<IdentityRemap>,
    ) -> Result<Self, ChangeSetError> {
        if from_revision == to_revision {
            return Err(ChangeSetError::RevisionDidNotAdvance);
        }
        if changed_sources.is_empty() && changed_objects.is_empty() && identity_remaps.is_empty() {
            return Err(ChangeSetError::NoChanges);
        }
        validate_collection_size("changed_sources", changed_sources.len())?;
        validate_collection_size("changed_objects", changed_objects.len())?;
        validate_collection_size("identity_remaps", identity_remaps.len())?;

        for source in &changed_sources {
            if source.workspace() != workspace {
                return Err(ChangeSetError::SourceWorkspaceMismatch {
                    expected: workspace,
                    actual: source.workspace(),
                });
            }
        }
        changed_sources.sort_unstable();
        changed_sources.dedup();

        for object in &changed_objects {
            if object.source().workspace() != workspace {
                return Err(ChangeSetError::ObjectWorkspaceMismatch {
                    expected: workspace,
                    actual: object.source().workspace(),
                });
            }
            if changed_sources.binary_search(&object.source()).is_err() {
                return Err(ChangeSetError::ObjectSourceNotChanged {
                    object: object.clone(),
                });
            }
        }
        changed_objects.sort_unstable();
        changed_objects.dedup();

        identity_remaps.sort_unstable();
        for pair in identity_remaps.windows(2) {
            if pair[0].from == pair[1].from && pair[0].to != pair[1].to {
                return Err(ChangeSetError::ConflictingIdentityRemap {
                    from: Box::new(pair[0].from.clone()),
                    first: Box::new(pair[0].to.clone()),
                    second: Box::new(pair[1].to.clone()),
                });
            }
        }
        identity_remaps.dedup();

        Ok(Self {
            transaction,
            workspace,
            from_revision,
            to_revision,
            changed_sources,
            changed_objects,
            identity_remaps,
        })
    }

    #[must_use]
    pub const fn transaction(&self) -> TransactionId {
        self.transaction
    }

    #[must_use]
    pub const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub const fn from_revision(&self) -> WorkspaceRevision {
        self.from_revision
    }

    #[must_use]
    pub const fn to_revision(&self) -> WorkspaceRevision {
        self.to_revision
    }

    #[must_use]
    pub fn changed_sources(&self) -> &[SourceId] {
        &self.changed_sources
    }

    #[must_use]
    pub fn changed_objects(&self) -> &[ObjectId] {
        &self.changed_objects
    }

    #[must_use]
    pub fn identity_remaps(&self) -> &[IdentityRemap] {
        &self.identity_remaps
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChangeSetError {
    #[error("change set version {0} is unsupported")]
    UnsupportedVersion(u8),
    #[error("change set revisions must advance")]
    RevisionDidNotAdvance,
    #[error("change set must contain at least one changed source, object, or identity remap")]
    NoChanges,
    #[error("change set {collection} contains {actual} items; maximum is {maximum}")]
    CollectionTooLarge {
        collection: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("identity remap source and target must differ")]
    IdentityDidNotChange,
    #[error("changed source belongs to workspace {actual}, not {expected}")]
    SourceWorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("changed object belongs to workspace {actual}, not {expected}")]
    ObjectWorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("changed object source is absent from changed_sources: {object:?}")]
    ObjectSourceNotChanged { object: ObjectId },
    #[error("identity {from:?} maps to both {first:?} and {second:?}")]
    ConflictingIdentityRemap {
        from: Box<ObjectAddress>,
        first: Box<ObjectAddress>,
        second: Box<ObjectAddress>,
    },
}

fn validate_collection_size(collection: &'static str, actual: usize) -> Result<(), ChangeSetError> {
    if actual > MAX_CHANGE_SET_ITEMS {
        Err(ChangeSetError::CollectionTooLarge {
            collection,
            actual,
            maximum: MAX_CHANGE_SET_ITEMS,
        })
    } else {
        Ok(())
    }
}
