use std::fmt;
use std::num::{NonZeroI64, NonZeroU128};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::bounded::{BoundedString, BoundedVec};
use crate::{SourceKind, WorkspaceRevision};

const WORKSPACE_PREFIX: &str = "workspace-v1:";
const ADDRESS_PREFIX: &str = "oa1:";
const WORKSPACE_ID_WIRE_BYTES: usize = WORKSPACE_PREFIX.len() + 32;
const MAX_SOURCE_ALIAS_BYTES: usize = 64 * 1024;
const MAX_MEMBER_PATH_BYTES: usize = 16 * 1024;
const MAX_YAML_ANCHOR_BYTES: usize = 1_024;
const MAX_CONTAINMENT_DEPTH: usize = 64;
const MAX_LOCATOR_TEXT_BYTES: usize = 96 * 1024;
const MAX_COMPACT_ADDRESS_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceId(NonZeroU128);

impl WorkspaceId {
    pub fn from_u128(value: u128) -> Result<Self, ContractError> {
        NonZeroU128::new(value)
            .map(Self)
            .ok_or(ContractError::ZeroWorkspaceId)
    }

    #[must_use]
    pub const fn get(self) -> u128 {
        self.0.get()
    }
}

impl fmt::Debug for WorkspaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{WORKSPACE_PREFIX}{:032x}", self.get())
    }
}

impl FromStr for WorkspaceId {
    type Err = ContractError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != WORKSPACE_ID_WIRE_BYTES {
            return Err(ContractError::InvalidWorkspaceIdLength {
                actual: value.len(),
                expected: WORKSPACE_ID_WIRE_BYTES,
            });
        }
        let encoded = value
            .strip_prefix(WORKSPACE_PREFIX)
            .ok_or_else(|| ContractError::InvalidWorkspaceId(value.to_owned()))?;
        if encoded.len() != 32 {
            return Err(ContractError::InvalidWorkspaceId(value.to_owned()));
        }
        let raw = u128::from_str_radix(encoded, 16)
            .map_err(|_| ContractError::InvalidWorkspaceId(value.to_owned()))?;
        Self::from_u128(raw)
    }
}

impl Serialize for WorkspaceId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for WorkspaceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        BoundedString::<WORKSPACE_ID_WIRE_BYTES>::deserialize(deserializer)?
            .into_string()
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Opaque identity of a source inside one workspace namespace.
///
/// The local value is derived from logical ownership, never from physical paths or fingerprints.
pub struct SourceId {
    workspace: WorkspaceId,
    kind: SourceKind,
    local: NonZeroU128,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SourceIdWire {
    version: u8,
    workspace: WorkspaceId,
    kind: SourceKind,
    local: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceIdInput {
    version: u8,
    workspace: WorkspaceId,
    kind: SourceKind,
    local: BoundedString<32>,
}

impl SourceId {
    pub fn new(
        workspace: WorkspaceId,
        kind: SourceKind,
        local: u128,
    ) -> Result<Self, ContractError> {
        let local = NonZeroU128::new(local).ok_or(ContractError::ZeroSourceId)?;
        Ok(Self {
            workspace,
            kind,
            local,
        })
    }

    #[must_use]
    pub const fn workspace(self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub const fn kind(self) -> SourceKind {
        self.kind
    }

    #[must_use]
    pub const fn local(self) -> u128 {
        self.local.get()
    }
}

impl Serialize for SourceId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SourceIdWire {
            version: 1,
            workspace: self.workspace,
            kind: self.kind,
            local: format!("{:032x}", self.local()),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SourceIdInput::deserialize(deserializer)?;
        validate_contract_version("source identity", wire.version)
            .map_err(serde::de::Error::custom)?;
        let local = wire.local.into_string();
        if local.len() != 32 {
            return Err(serde::de::Error::custom(ContractError::InvalidSourceId(
                local,
            )));
        }
        let parsed = u128::from_str_radix(&local, 16)
            .map_err(|_| serde::de::Error::custom(ContractError::InvalidSourceId(local.clone())))?;
        Self::new(wire.workspace, wire.kind, parsed).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Portable name that binds a logical root source to a runtime physical origin.
///
/// Aliases are relative, slash-separated, and deliberately independent of host path semantics.
pub struct SourceAlias(String);

impl SourceAlias {
    pub fn new(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        validate_portable_path(&value, "source alias", MAX_SOURCE_ALIAS_BYTES)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceAlias {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for SourceAlias {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SourceAlias {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(BoundedString::<MAX_SOURCE_ALIAS_BYTES>::deserialize(deserializer)?.into_string())
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Stable member identity inside a container.
///
/// Parsed-vector indexes are not identities. Duplicate names use an explicit same-name occurrence.
pub struct SourceMemberId {
    name: String,
    same_name_occurrence: u32,
}

impl SourceMemberId {
    pub fn new(name: impl Into<String>) -> Result<Self, ContractError> {
        Self::with_occurrence(name, 0)
    }

    pub fn with_occurrence(
        name: impl Into<String>,
        same_name_occurrence: u32,
    ) -> Result<Self, ContractError> {
        let name = name.into();
        validate_portable_path(&name, "source member", MAX_MEMBER_PATH_BYTES)?;
        Ok(Self {
            name,
            same_name_occurrence,
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn same_name_occurrence(&self) -> u32 {
        self.same_name_occurrence
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SourceMemberRef<'a> {
    name: &'a str,
    same_name_occurrence: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceMemberWire {
    name: BoundedString<MAX_MEMBER_PATH_BYTES>,
    same_name_occurrence: u32,
}

impl Serialize for SourceMemberId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SourceMemberRef {
            name: &self.name,
            same_name_occurrence: self.same_name_occurrence,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SourceMemberId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SourceMemberWire::deserialize(deserializer)?;
        Self::with_occurrence(wire.name.into_string(), wire.same_name_occurrence)
            .map_err(serde::de::Error::custom)
    }
}

pub type BundleMemberId = SourceMemberId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Container relationship represented by one step in a logical source locator.
pub enum ContainmentKind {
    Archive,
    WebFile,
    Bundle,
}

impl ContainmentKind {
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::WebFile => "web_file",
            Self::Bundle => "bundle",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// One validated parent-to-member relationship in a source ownership chain.
pub struct ContainmentStep {
    container: ContainmentKind,
    member: SourceMemberId,
}

impl ContainmentStep {
    #[must_use]
    pub const fn new(container: ContainmentKind, member: SourceMemberId) -> Self {
        Self { container, member }
    }

    #[must_use]
    pub const fn container(&self) -> ContainmentKind {
        self.container
    }

    #[must_use]
    pub fn member(&self) -> &SourceMemberId {
        &self.member
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.member.name()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Portable source locator: a root alias followed by exact containment steps.
///
/// This type is safe to persist. Runtime filesystem paths live in `SourceCatalog` instead.
pub struct SourceLocator {
    root_alias: SourceAlias,
    members: Vec<ContainmentStep>,
}

impl SourceLocator {
    pub fn path(alias: impl Into<String>) -> Result<Self, ContractError> {
        let locator = Self {
            root_alias: SourceAlias::new(alias)?,
            members: Vec::new(),
        };
        locator.validate_total_size()?;
        Ok(locator)
    }

    pub fn archive_member(
        root_alias: impl Into<String>,
        entry_name: impl Into<String>,
    ) -> Result<Self, ContractError> {
        Self::path(root_alias)?.child(ContainmentKind::Archive, SourceMemberId::new(entry_name)?)
    }

    pub fn webfile_member(
        root_alias: impl Into<String>,
        entry_name: impl Into<String>,
    ) -> Result<Self, ContractError> {
        Self::path(root_alias)?.child(ContainmentKind::WebFile, SourceMemberId::new(entry_name)?)
    }

    pub fn child(
        mut self,
        container: ContainmentKind,
        member: SourceMemberId,
    ) -> Result<Self, ContractError> {
        if self.members.len() == MAX_CONTAINMENT_DEPTH {
            return Err(ContractError::ContainmentDepthExceeded {
                max_depth: MAX_CONTAINMENT_DEPTH,
            });
        }
        self.members.push(ContainmentStep::new(container, member));
        self.validate_total_size()?;
        Ok(self)
    }

    #[must_use]
    pub fn root_alias(&self) -> &SourceAlias {
        &self.root_alias
    }

    #[must_use]
    pub fn members(&self) -> &[ContainmentStep] {
        &self.members
    }

    #[must_use]
    pub fn bundle_member(&self) -> Option<&BundleMemberId> {
        self.members
            .last()
            .and_then(|step| (step.container == ContainmentKind::Bundle).then_some(&step.member))
    }

    #[must_use]
    fn last_containment_kind(&self) -> Option<ContainmentKind> {
        self.members.last().map(ContainmentStep::container)
    }

    pub(crate) fn from_parts(
        root_alias: SourceAlias,
        members: Vec<ContainmentStep>,
    ) -> Result<Self, ContractError> {
        if members.len() > MAX_CONTAINMENT_DEPTH {
            return Err(ContractError::ContainmentDepthExceeded {
                max_depth: MAX_CONTAINMENT_DEPTH,
            });
        }
        let locator = Self {
            root_alias,
            members,
        };
        locator.validate_total_size()?;
        Ok(locator)
    }

    fn validate_total_size(&self) -> Result<(), ContractError> {
        let total_bytes = self
            .members
            .iter()
            .try_fold(self.root_alias.as_str().len(), |total, step| {
                total.checked_add(step.member().name().len())
            });
        if total_bytes.is_none_or(|total| total > MAX_LOCATOR_TEXT_BYTES) {
            return Err(ContractError::SourceLocatorTooLong {
                max_text_bytes: MAX_LOCATOR_TEXT_BYTES,
            });
        }
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SourceLocatorRef<'a> {
    version: u8,
    outer_path: &'a SourceAlias,
    members: &'a [ContainmentStep],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceLocatorWire {
    version: u8,
    outer_path: SourceAlias,
    members: BoundedVec<ContainmentStep, MAX_CONTAINMENT_DEPTH>,
}

impl Serialize for SourceLocator {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SourceLocatorRef {
            version: 1,
            outer_path: &self.root_alias,
            members: &self.members,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SourceLocator {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SourceLocatorWire::deserialize(deserializer)?;
        validate_contract_version("source locator", wire.version)
            .map_err(serde::de::Error::custom)?;
        Self::from_parts(wire.outer_path, wire.members.into_vec()).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Validated YAML anchor spelling. The string `"0"` is a valid anchor.
pub struct YamlAnchor(String);

impl YamlAnchor {
    pub fn new(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_YAML_ANCHOR_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(ContractError::InvalidYamlAnchor(value));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for YamlAnchor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for YamlAnchor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for YamlAnchor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(BoundedString::<MAX_YAML_ANCHOR_BYTES>::deserialize(deserializer)?.into_string())
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
/// Stable YAML document selector that keeps real anchors distinct from unanchored ordinals.
pub enum YamlDocumentSelector {
    Anchored { anchor: YamlAnchor },
    Unanchored { document_index: u32 },
}

impl YamlDocumentSelector {
    pub fn anchor(value: impl Into<String>) -> Result<Self, ContractError> {
        Ok(Self::Anchored {
            anchor: YamlAnchor::new(value)?,
        })
    }

    #[must_use]
    pub const fn ordinal(document_index: u32) -> Self {
        Self::Unanchored { document_index }
    }

    #[must_use]
    pub fn anchor_value(&self) -> Option<&YamlAnchor> {
        match self {
            Self::Anchored { anchor, .. } => Some(anchor),
            Self::Unanchored { .. } => None,
        }
    }

    #[must_use]
    pub fn anchor_str(&self) -> Option<&str> {
        self.anchor_value().map(YamlAnchor::as_str)
    }

    #[must_use]
    pub const fn ordinal_index(&self) -> Option<u32> {
        match self {
            Self::Anchored { .. } => None,
            Self::Unanchored { document_index } => Some(*document_index),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Format-local object identity family.
pub enum ObjectKind {
    Binary,
    Yaml,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ObjectKey {
    BinaryPathId(NonZeroI64),
    YamlAnchor(YamlAnchor),
    YamlDocumentOrdinal(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Workspace-local object identity composed only from an owning source and a local object key.
pub struct ObjectId {
    source: SourceId,
    key: ObjectKey,
}

impl ObjectId {
    pub fn binary(source: SourceId, path_id: i64) -> Result<Self, ContractError> {
        validate_object_source_kind(source, SourceKind::SerializedFile)?;
        Ok(Self {
            source,
            key: ObjectKey::BinaryPathId(
                NonZeroI64::new(path_id).ok_or(ContractError::NullBinaryObjectId)?,
            ),
        })
    }

    pub fn yaml(source: SourceId, anchor: impl Into<String>) -> Result<Self, ContractError> {
        validate_object_source_kind(source, SourceKind::Yaml)?;
        Ok(Self {
            source,
            key: ObjectKey::YamlAnchor(YamlAnchor::new(anchor)?),
        })
    }

    pub fn yaml_document(source: SourceId, document_index: u32) -> Result<Self, ContractError> {
        validate_object_source_kind(source, SourceKind::Yaml)?;
        Ok(Self {
            source,
            key: ObjectKey::YamlDocumentOrdinal(document_index),
        })
    }

    pub fn from_yaml_selector(
        source: SourceId,
        selector: &YamlDocumentSelector,
    ) -> Result<Self, ContractError> {
        match selector {
            YamlDocumentSelector::Anchored { anchor, .. } => Self::yaml(source, anchor.as_str()),
            YamlDocumentSelector::Unanchored { document_index } => {
                Self::yaml_document(source, *document_index)
            }
        }
    }

    #[must_use]
    pub const fn source(&self) -> SourceId {
        self.source
    }

    #[must_use]
    pub fn kind(&self) -> ObjectKind {
        match &self.key {
            ObjectKey::BinaryPathId(_) => ObjectKind::Binary,
            ObjectKey::YamlAnchor(_) | ObjectKey::YamlDocumentOrdinal(_) => ObjectKind::Yaml,
        }
    }

    #[must_use]
    pub fn binary_path_id(&self) -> Option<i64> {
        match &self.key {
            ObjectKey::BinaryPathId(value) => Some(value.get()),
            ObjectKey::YamlAnchor(_) | ObjectKey::YamlDocumentOrdinal(_) => None,
        }
    }

    #[must_use]
    pub fn yaml_anchor(&self) -> Option<&str> {
        match &self.key {
            ObjectKey::YamlAnchor(value) => Some(value.as_str()),
            ObjectKey::BinaryPathId(_) | ObjectKey::YamlDocumentOrdinal(_) => None,
        }
    }

    #[must_use]
    pub fn yaml_document_ordinal(&self) -> Option<u32> {
        match &self.key {
            ObjectKey::YamlDocumentOrdinal(index) => Some(*index),
            ObjectKey::BinaryPathId(_) | ObjectKey::YamlAnchor(_) => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ObjectIdWire {
    Binary {
        version: u8,
        source: SourceId,
        path_id: i64,
    },
    Yaml {
        version: u8,
        source: SourceId,
        selector: YamlDocumentSelector,
    },
}

impl From<ObjectId> for ObjectIdWire {
    fn from(value: ObjectId) -> Self {
        match value.key {
            ObjectKey::BinaryPathId(path_id) => Self::Binary {
                version: 1,
                source: value.source,
                path_id: path_id.get(),
            },
            ObjectKey::YamlAnchor(anchor) => Self::Yaml {
                version: 1,
                source: value.source,
                selector: YamlDocumentSelector::Anchored { anchor },
            },
            ObjectKey::YamlDocumentOrdinal(document_index) => Self::Yaml {
                version: 1,
                source: value.source,
                selector: YamlDocumentSelector::Unanchored { document_index },
            },
        }
    }
}

impl TryFrom<ObjectIdWire> for ObjectId {
    type Error = ContractError;

    fn try_from(value: ObjectIdWire) -> Result<Self, Self::Error> {
        match value {
            ObjectIdWire::Binary {
                version,
                source,
                path_id,
            } => {
                validate_contract_version("object identity", version)?;
                Self::binary(source, path_id)
            }
            ObjectIdWire::Yaml {
                version,
                source,
                selector,
            } => {
                validate_contract_version("object identity", version)?;
                Self::from_yaml_selector(source, &selector)
            }
        }
    }
}

impl Serialize for ObjectId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ObjectIdWire::from(self.clone()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ObjectIdWire::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// In-process object handle that rejects use outside its workspace revision.
pub struct RevisionedObjectHandle {
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    object: ObjectId,
}

impl RevisionedObjectHandle {
    pub fn new(
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        object: ObjectId,
    ) -> Result<Self, ContractError> {
        if object.source().workspace() != workspace {
            return Err(ContractError::ObjectWorkspaceMismatch {
                handle_workspace: workspace,
                object_workspace: object.source().workspace(),
            });
        }
        Ok(Self {
            workspace,
            revision,
            object,
        })
    }

    pub fn validate_context(
        &self,
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
    ) -> Result<(), ContractError> {
        if self.workspace != workspace {
            return Err(ContractError::WorkspaceMismatch {
                expected: self.workspace,
                actual: workspace,
            });
        }
        if self.revision != revision {
            return Err(ContractError::RevisionMismatch {
                expected: self.revision,
                actual: revision,
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn object(&self) -> &ObjectId {
        &self.object
    }

    #[must_use]
    pub const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ObjectAddressKey {
    BinaryPathId(NonZeroI64),
    Yaml(YamlDocumentSelector),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Versioned, serializable object locator for plans, CLI input, and commit reports.
pub struct ObjectAddress {
    source: SourceLocator,
    key: ObjectAddressKey,
}

impl ObjectAddress {
    pub fn binary_at(source: SourceLocator, path_id: i64) -> Result<Self, ContractError> {
        Ok(Self {
            source,
            key: ObjectAddressKey::BinaryPathId(
                NonZeroI64::new(path_id).ok_or(ContractError::NullBinaryObjectId)?,
            ),
        })
    }

    pub fn binary_direct(locator: SourceLocator, path_id: i64) -> Result<Self, ContractError> {
        if locator.last_containment_kind() == Some(ContainmentKind::Bundle) {
            return Err(ContractError::DirectAddressContainsBundleMember);
        }
        Self::binary_at(locator, path_id)
    }

    pub fn binary_bundle_member(
        locator: SourceLocator,
        member: BundleMemberId,
        path_id: i64,
    ) -> Result<Self, ContractError> {
        Self::binary_at(locator.child(ContainmentKind::Bundle, member)?, path_id)
    }

    pub fn yaml(locator: SourceLocator, anchor: impl Into<String>) -> Result<Self, ContractError> {
        Self::yaml_with_selector(locator, YamlDocumentSelector::anchor(anchor)?)
    }

    pub fn yaml_document(
        locator: SourceLocator,
        document_index: u32,
    ) -> Result<Self, ContractError> {
        Self::yaml_with_selector(locator, YamlDocumentSelector::ordinal(document_index))
    }

    pub fn yaml_with_selector(
        source: SourceLocator,
        selector: YamlDocumentSelector,
    ) -> Result<Self, ContractError> {
        Ok(Self {
            source,
            key: ObjectAddressKey::Yaml(selector),
        })
    }

    #[must_use]
    pub fn kind(&self) -> ObjectKind {
        match &self.key {
            ObjectAddressKey::BinaryPathId(_) => ObjectKind::Binary,
            ObjectAddressKey::Yaml(_) => ObjectKind::Yaml,
        }
    }

    #[must_use]
    pub fn binary_path_id(&self) -> Option<i64> {
        match &self.key {
            ObjectAddressKey::BinaryPathId(path_id) => Some(path_id.get()),
            ObjectAddressKey::Yaml(_) => None,
        }
    }

    #[must_use]
    pub fn yaml_selector(&self) -> Option<&YamlDocumentSelector> {
        match &self.key {
            ObjectAddressKey::BinaryPathId(_) => None,
            ObjectAddressKey::Yaml(selector) => Some(selector),
        }
    }

    #[must_use]
    pub fn yaml_anchor(&self) -> Option<&str> {
        self.yaml_selector()
            .and_then(YamlDocumentSelector::anchor_str)
    }

    #[must_use]
    pub fn yaml_document_ordinal(&self) -> Option<u32> {
        self.yaml_selector()
            .and_then(YamlDocumentSelector::ordinal_index)
    }

    #[must_use]
    pub const fn source_locator(&self) -> &SourceLocator {
        &self.source
    }

    #[must_use]
    pub fn bundle_member(&self) -> Option<&BundleMemberId> {
        self.source.bundle_member()
    }

    pub fn to_compact_string(&self) -> Result<String, ContractError> {
        let json = serde_json::to_vec(self)
            .map_err(|error| ContractError::CompactAddress(error.to_string()))?;
        if json.len() > MAX_COMPACT_ADDRESS_BYTES {
            return Err(ContractError::CompactAddressTooLong {
                encoded_bytes: json.len().saturating_mul(2),
                max_encoded_bytes: MAX_COMPACT_ADDRESS_BYTES * 2,
            });
        }
        Ok(format!("{ADDRESS_PREFIX}{}", hex::encode(json)))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ObjectAddressWire {
    BinaryDirect {
        version: u8,
        source: SourceLocator,
        path_id: i64,
    },
    BinaryBundleMember {
        version: u8,
        source: SourceLocator,
        path_id: i64,
    },
    Yaml {
        version: u8,
        source: SourceLocator,
        selector: YamlDocumentSelector,
    },
}

impl From<ObjectAddress> for ObjectAddressWire {
    fn from(value: ObjectAddress) -> Self {
        match value.key {
            ObjectAddressKey::BinaryPathId(path_id) => {
                if value.source.last_containment_kind() == Some(ContainmentKind::Bundle) {
                    Self::BinaryBundleMember {
                        version: 1,
                        source: value.source,
                        path_id: path_id.get(),
                    }
                } else {
                    Self::BinaryDirect {
                        version: 1,
                        source: value.source,
                        path_id: path_id.get(),
                    }
                }
            }
            ObjectAddressKey::Yaml(selector) => Self::Yaml {
                version: 1,
                source: value.source,
                selector,
            },
        }
    }
}

impl TryFrom<ObjectAddressWire> for ObjectAddress {
    type Error = ContractError;

    fn try_from(value: ObjectAddressWire) -> Result<Self, Self::Error> {
        match value {
            ObjectAddressWire::BinaryDirect {
                version,
                source,
                path_id,
            } => {
                validate_contract_version("object address", version)?;
                Self::binary_direct(source, path_id)
            }
            ObjectAddressWire::BinaryBundleMember {
                version,
                source,
                path_id,
            } => {
                validate_contract_version("object address", version)?;
                if source.last_containment_kind() != Some(ContainmentKind::Bundle) {
                    return Err(ContractError::BundleAddressMissingMember);
                }
                Self::binary_at(source, path_id)
            }
            ObjectAddressWire::Yaml {
                version,
                source,
                selector,
            } => {
                validate_contract_version("object address", version)?;
                Self::yaml_with_selector(source, selector)
            }
        }
    }
}

impl Serialize for ObjectAddress {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ObjectAddressWire::from(self.clone()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ObjectAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ObjectAddressWire::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

impl FromStr for ObjectAddress {
    type Err = ContractError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let encoded = value
            .strip_prefix(ADDRESS_PREFIX)
            .ok_or_else(|| ContractError::CompactAddress("unsupported address prefix".into()))?;
        let max_encoded_bytes = MAX_COMPACT_ADDRESS_BYTES * 2;
        if encoded.len() > max_encoded_bytes {
            return Err(ContractError::CompactAddressTooLong {
                encoded_bytes: encoded.len(),
                max_encoded_bytes,
            });
        }
        if encoded.len() % 2 != 0 {
            return Err(ContractError::CompactAddress(
                "hex payload has an odd length".into(),
            ));
        }
        let json = hex::decode(encoded)
            .map_err(|error| ContractError::CompactAddress(error.to_string()))?;
        serde_json::from_slice(&json)
            .map_err(|error| ContractError::CompactAddress(error.to_string()))
    }
}

fn validate_contract_version(contract: &'static str, version: u8) -> Result<(), ContractError> {
    if version == 1 {
        Ok(())
    } else {
        Err(ContractError::UnsupportedContractVersion { contract, version })
    }
}

fn validate_object_source_kind(
    source: SourceId,
    expected: SourceKind,
) -> Result<(), ContractError> {
    if source.kind() == expected {
        Ok(())
    } else {
        Err(ContractError::ObjectSourceKindMismatch {
            expected,
            actual: source.kind(),
        })
    }
}

fn validate_portable_path(
    value: &str,
    kind: &'static str,
    max_bytes: usize,
) -> Result<(), ContractError> {
    let has_drive_prefix = value.as_bytes().get(1) == Some(&b':')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic);
    let invalid = value.is_empty()
        || value.len() > max_bytes
        || value.starts_with('/')
        || value.contains('\\')
        || has_drive_prefix
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..");
    if invalid {
        Err(ContractError::InvalidPortablePath {
            kind,
            value: value.to_owned(),
        })
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ContractError {
    #[error("workspace id must be nonzero")]
    ZeroWorkspaceId,
    #[error("invalid workspace id: {0}")]
    InvalidWorkspaceId(String),
    #[error("workspace id has {actual} encoded bytes; expected {expected}")]
    InvalidWorkspaceIdLength { actual: usize, expected: usize },
    #[error("source id must be nonzero")]
    ZeroSourceId,
    #[error("invalid source id: {0}")]
    InvalidSourceId(String),
    #[error("binary pathID zero denotes null and cannot identify an object")]
    NullBinaryObjectId,
    #[error("direct SerializedFile address contains a bundle member step")]
    DirectAddressContainsBundleMember,
    #[error("bundle-member address is missing its bundle member identity")]
    BundleAddressMissingMember,
    #[error("{kind} is not a valid portable path: {value:?}")]
    InvalidPortablePath { kind: &'static str, value: String },
    #[error("invalid YAML anchor: {0:?}")]
    InvalidYamlAnchor(String),
    #[error("source containment exceeds the maximum depth of {max_depth}")]
    ContainmentDepthExceeded { max_depth: usize },
    #[error("source locator text exceeds the maximum of {max_text_bytes} bytes")]
    SourceLocatorTooLong { max_text_bytes: usize },
    #[error("{contract} version {version} is unsupported")]
    UnsupportedContractVersion { contract: &'static str, version: u8 },
    #[error("invalid compact object address: {0}")]
    CompactAddress(String),
    #[error(
        "compact object address has {encoded_bytes} encoded bytes; maximum is {max_encoded_bytes}"
    )]
    CompactAddressTooLong {
        encoded_bytes: usize,
        max_encoded_bytes: usize,
    },
    #[error("object requires source kind {expected:?}, got {actual:?}")]
    ObjectSourceKindMismatch {
        expected: SourceKind,
        actual: SourceKind,
    },
    #[error("workspace mismatch: expected {expected}, got {actual}")]
    WorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("workspace revision mismatch: expected {expected}, got {actual}")]
    RevisionMismatch {
        expected: WorkspaceRevision,
        actual: WorkspaceRevision,
    },
    #[error(
        "object belongs to workspace {object_workspace}, not handle workspace {handle_workspace}"
    )]
    ObjectWorkspaceMismatch {
        handle_workspace: WorkspaceId,
        object_workspace: WorkspaceId,
    },
}
