//! Versioned, deterministic extraction requests and inert plans.

use std::cmp::Ordering;
use std::io::{Read, Write};
use std::num::NonZeroU64;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use unity_asset_binary::unity_version::{UnityVersion, UnityVersionType};
use unity_asset_core::{
    AssetLoadBudget, BudgetedJsonError, DigestV1, ObjectAddress, ObjectKind, SourceFingerprint,
    SourceKind, SourceLocator, WorkspaceId, WorkspaceRevision,
};
use unity_asset_write::artifact::{ArtifactNameError, LogicalArtifactName};

use super::manifest::{
    ExtractionCanonicalError, canonical_digest, canonical_json, read_json_bounded,
    write_canonical_json,
};
pub use super::manifest::{ExtractionDiagnostic, ExtractionDiagnosticCode};

pub const EXTRACTION_REQUEST_VERSION: u8 = 1;
pub const EXTRACTION_PLAN_VERSION: u8 = 1;
pub const EXTRACTION_MANIFEST_VERSION: u8 = super::manifest::EXTRACTION_MANIFEST_VERSION;
pub const EXTRACTION_REPORT_VERSION: u8 = super::manifest::EXTRACTION_REPORT_VERSION;
pub const EXTRACTION_REQUEST_CONTRACT: &str = "unity_asset.extraction_request";
pub const EXTRACTION_PLAN_CONTRACT: &str = "unity_asset.extraction_plan";

const MAX_SELECTION_PATTERN_BYTES: usize = 4_096;
const MAX_FILTER_TEXT_BYTES: usize = 4_096;
const MAX_METADATA_TEXT_BYTES: usize = 64 * 1_024;
const MAX_AUDIO_EXTENSION_BYTES: usize = 16;

/// A validated relative output path that has portable filesystem semantics.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtractionPath {
    value: String,
    portability_key: String,
}

impl ExtractionPath {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ExtractionModelError> {
        let name = LogicalArtifactName::new(value.as_ref())?;
        let value = name.as_str().to_owned();
        let portability_key = name.portability_key().to_owned();
        Ok(Self {
            value,
            portability_key,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub(crate) fn portability_key(&self) -> &str {
        &self.portability_key
    }
}

impl PartialOrd for ExtractionPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ExtractionPath {
    fn cmp(&self, other: &Self) -> Ordering {
        self.value.cmp(&other.value)
    }
}

impl AsRef<str> for ExtractionPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Serialize for ExtractionPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ExtractionPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// The requested logical representation of extracted objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionRepresentationPolicy {
    RawOnly,
    PreferDecoded,
    RequireDecoded,
}

/// A portable, normalized object selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractionSelection {
    All,
    Sources {
        sources: Box<[SourceLocator]>,
    },
    Addresses {
        addresses: Box<[ObjectAddress]>,
    },
    BundleContainer {
        pattern: String,
        addresses: Box<[ObjectAddress]>,
    },
    ReferenceTraversal {
        addresses: Box<[ObjectAddress]>,
    },
}

impl ExtractionSelection {
    fn normalize(self) -> Result<Self, ExtractionModelError> {
        match self {
            Self::All => Ok(Self::All),
            Self::Sources { sources } => Ok(Self::Sources {
                sources: normalize_values(sources.into_vec()).into_boxed_slice(),
            }),
            Self::Addresses { addresses } => Ok(Self::Addresses {
                addresses: normalize_values(addresses.into_vec()).into_boxed_slice(),
            }),
            Self::BundleContainer { pattern, addresses } => {
                let pattern = normalize_selection_pattern(pattern)?;
                Ok(Self::BundleContainer {
                    pattern,
                    addresses: normalize_values(addresses.into_vec()).into_boxed_slice(),
                })
            }
            Self::ReferenceTraversal { addresses } => Ok(Self::ReferenceTraversal {
                addresses: normalize_values(addresses.into_vec()).into_boxed_slice(),
            }),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExtractionSelectionRef<'value> {
    All,
    Sources {
        sources: &'value [SourceLocator],
    },
    Addresses {
        addresses: &'value [ObjectAddress],
    },
    BundleContainer {
        pattern: &'value str,
        addresses: &'value [ObjectAddress],
    },
    ReferenceTraversal {
        addresses: &'value [ObjectAddress],
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ExtractionSelectionWire {
    All,
    Sources {
        sources: Vec<SourceLocator>,
    },
    Addresses {
        addresses: Vec<ObjectAddress>,
    },
    BundleContainer {
        pattern: String,
        addresses: Vec<ObjectAddress>,
    },
    ReferenceTraversal {
        addresses: Vec<ObjectAddress>,
    },
}

impl Serialize for ExtractionSelection {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let selection = match self {
            Self::All => ExtractionSelectionRef::All,
            Self::Sources { sources } => ExtractionSelectionRef::Sources { sources },
            Self::Addresses { addresses } => ExtractionSelectionRef::Addresses { addresses },
            Self::BundleContainer { pattern, addresses } => {
                ExtractionSelectionRef::BundleContainer { pattern, addresses }
            }
            Self::ReferenceTraversal { addresses } => {
                ExtractionSelectionRef::ReferenceTraversal { addresses }
            }
        };
        selection.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExtractionSelection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let selection = match ExtractionSelectionWire::deserialize(deserializer)? {
            ExtractionSelectionWire::All => Self::All,
            ExtractionSelectionWire::Sources { sources } => Self::Sources {
                sources: sources.into_boxed_slice(),
            },
            ExtractionSelectionWire::Addresses { addresses } => Self::Addresses {
                addresses: addresses.into_boxed_slice(),
            },
            ExtractionSelectionWire::BundleContainer { pattern, addresses } => {
                Self::BundleContainer {
                    pattern,
                    addresses: addresses.into_boxed_slice(),
                }
            }
            ExtractionSelectionWire::ReferenceTraversal { addresses } => Self::ReferenceTraversal {
                addresses: addresses.into_boxed_slice(),
            },
        };
        selection.normalize().map_err(serde::de::Error::custom)
    }
}

/// Optional filters applied after a selection adapter resolves candidate objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionFilter {
    class_ids: Box<[i32]>,
    class_name_contains: Option<String>,
    object_name_contains: Option<String>,
    limit: Option<NonZeroU64>,
}

impl ExtractionFilter {
    pub fn new(
        class_ids: impl IntoIterator<Item = i32>,
        class_name_contains: Option<String>,
        object_name_contains: Option<String>,
        limit: Option<u64>,
    ) -> Result<Self, ExtractionModelError> {
        let limit = limit
            .map(|value| NonZeroU64::new(value).ok_or(ExtractionModelError::ZeroExtractionLimit))
            .transpose()?;
        Ok(Self {
            class_ids: normalize_values(class_ids.into_iter().collect()).into_boxed_slice(),
            class_name_contains: normalize_filter_text("class_name_contains", class_name_contains)?,
            object_name_contains: normalize_filter_text(
                "object_name_contains",
                object_name_contains,
            )?,
            limit,
        })
    }

    #[must_use]
    pub fn class_ids(&self) -> &[i32] {
        &self.class_ids
    }

    #[must_use]
    pub fn class_name_contains(&self) -> Option<&str> {
        self.class_name_contains.as_deref()
    }

    #[must_use]
    pub fn object_name_contains(&self) -> Option<&str> {
        self.object_name_contains.as_deref()
    }

    #[must_use]
    pub fn limit(&self) -> Option<u64> {
        self.limit.map(NonZeroU64::get)
    }

    #[must_use]
    pub fn matches_class(&self, class_id: i32, class_name: &str) -> bool {
        (self.class_ids.is_empty() || self.class_ids.binary_search(&class_id).is_ok())
            && self
                .class_name_contains
                .as_deref()
                .is_none_or(|needle| lowercase_contains(class_name, needle))
    }

    #[must_use]
    pub fn matches_object_name(&self, object_name: Option<&str>) -> bool {
        self.object_name_contains
            .as_deref()
            .is_none_or(|needle| object_name.is_some_and(|name| lowercase_contains(name, needle)))
    }
}

impl Default for ExtractionFilter {
    fn default() -> Self {
        Self {
            class_ids: Box::new([]),
            class_name_contains: None,
            object_name_contains: None,
            limit: None,
        }
    }
}

#[derive(Serialize)]
struct ExtractionFilterRef<'value> {
    class_ids: &'value [i32],
    class_name_contains: Option<&'value str>,
    object_name_contains: Option<&'value str>,
    limit: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractionFilterWire {
    class_ids: Vec<i32>,
    class_name_contains: Option<String>,
    object_name_contains: Option<String>,
    limit: Option<u64>,
}

impl Serialize for ExtractionFilter {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ExtractionFilterRef {
            class_ids: &self.class_ids,
            class_name_contains: self.class_name_contains(),
            object_name_contains: self.object_name_contains(),
            limit: self.limit(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExtractionFilter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ExtractionFilterWire::deserialize(deserializer)?;
        Self::new(
            wire.class_ids,
            wire.class_name_contains,
            wire.object_name_contains,
            wire.limit,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// A versioned extraction request containing only portable identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionRequest {
    selection: ExtractionSelection,
    representation: ExtractionRepresentationPolicy,
    filter: ExtractionFilter,
    prefix: Option<ExtractionPath>,
}

impl ExtractionRequest {
    #[must_use]
    pub fn all(representation: ExtractionRepresentationPolicy) -> Self {
        Self {
            selection: ExtractionSelection::All,
            representation,
            filter: ExtractionFilter::default(),
            prefix: None,
        }
    }

    #[must_use]
    pub fn sources(
        sources: impl IntoIterator<Item = SourceLocator>,
        representation: ExtractionRepresentationPolicy,
    ) -> Self {
        Self {
            selection: ExtractionSelection::Sources {
                sources: normalize_values(sources.into_iter().collect()).into_boxed_slice(),
            },
            representation,
            filter: ExtractionFilter::default(),
            prefix: None,
        }
    }

    pub fn addresses(
        addresses: impl IntoIterator<Item = ObjectAddress>,
        representation: ExtractionRepresentationPolicy,
    ) -> Result<Self, ExtractionModelError> {
        Self::with_selection(
            ExtractionSelection::Addresses {
                addresses: addresses.into_iter().collect::<Vec<_>>().into_boxed_slice(),
            },
            representation,
            ExtractionFilter::default(),
            None,
        )
    }

    pub fn bundle_container(
        pattern: impl Into<String>,
        addresses: impl IntoIterator<Item = ObjectAddress>,
        representation: ExtractionRepresentationPolicy,
    ) -> Result<Self, ExtractionModelError> {
        Self::with_selection(
            ExtractionSelection::BundleContainer {
                pattern: pattern.into(),
                addresses: addresses.into_iter().collect::<Vec<_>>().into_boxed_slice(),
            },
            representation,
            ExtractionFilter::default(),
            None,
        )
    }

    #[must_use]
    pub fn reference_traversal(
        addresses: impl IntoIterator<Item = ObjectAddress>,
        representation: ExtractionRepresentationPolicy,
    ) -> Self {
        Self {
            selection: ExtractionSelection::ReferenceTraversal {
                addresses: normalize_values(addresses.into_iter().collect()).into_boxed_slice(),
            },
            representation,
            filter: ExtractionFilter::default(),
            prefix: None,
        }
    }

    #[must_use]
    pub fn with_filter(mut self, filter: ExtractionFilter) -> Self {
        self.filter = filter;
        self
    }

    #[must_use]
    pub fn with_prefix(mut self, prefix: ExtractionPath) -> Self {
        self.prefix = Some(prefix);
        self
    }

    pub(crate) fn with_selection(
        selection: ExtractionSelection,
        representation: ExtractionRepresentationPolicy,
        filter: ExtractionFilter,
        prefix: Option<ExtractionPath>,
    ) -> Result<Self, ExtractionModelError> {
        Ok(Self {
            selection: selection.normalize()?,
            representation,
            filter,
            prefix,
        })
    }

    #[must_use]
    pub const fn selection(&self) -> &ExtractionSelection {
        &self.selection
    }

    #[must_use]
    pub const fn representation(&self) -> ExtractionRepresentationPolicy {
        self.representation
    }

    #[must_use]
    pub const fn filter(&self) -> &ExtractionFilter {
        &self.filter
    }

    #[must_use]
    pub fn prefix(&self) -> Option<&ExtractionPath> {
        self.prefix.as_ref()
    }

    pub fn read_json(
        reader: impl Read,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, BudgetedJsonError> {
        read_json_bounded(reader, budget)
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, ExtractionCanonicalError> {
        canonical_json(self)
    }

    pub fn write_canonical_json(&self, writer: impl Write) -> Result<(), ExtractionCanonicalError> {
        write_canonical_json(writer, self)
    }

    pub fn digest(&self) -> Result<DigestV1, ExtractionCanonicalError> {
        canonical_digest(self)
    }
}

#[derive(Serialize)]
struct ExtractionRequestRef<'value> {
    contract: &'static str,
    version: u8,
    selection: &'value ExtractionSelection,
    representation: ExtractionRepresentationPolicy,
    filter: &'value ExtractionFilter,
    prefix: Option<&'value ExtractionPath>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractionRequestWire {
    contract: String,
    version: u8,
    selection: ExtractionSelection,
    representation: ExtractionRepresentationPolicy,
    filter: ExtractionFilter,
    prefix: Option<ExtractionPath>,
}

impl Serialize for ExtractionRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ExtractionRequestRef {
            contract: EXTRACTION_REQUEST_CONTRACT,
            version: EXTRACTION_REQUEST_VERSION,
            selection: &self.selection,
            representation: self.representation,
            filter: &self.filter,
            prefix: self.prefix.as_ref(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExtractionRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ExtractionRequestWire::deserialize(deserializer)?;
        if wire.contract != EXTRACTION_REQUEST_CONTRACT {
            return Err(serde::de::Error::custom(
                ExtractionModelError::UnexpectedContract {
                    expected: EXTRACTION_REQUEST_CONTRACT,
                    actual: wire.contract,
                },
            ));
        }
        if wire.version != EXTRACTION_REQUEST_VERSION {
            return Err(serde::de::Error::custom(
                ExtractionModelError::UnsupportedRequestVersion(wire.version),
            ));
        }
        Self::with_selection(
            wire.selection,
            wire.representation,
            wire.filter,
            wire.prefix,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// One source byte range required by a decoded representation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExtractionSourceRange {
    source: SourceLocator,
    offset: u64,
    size: u64,
}

impl ExtractionSourceRange {
    pub fn new(
        source: SourceLocator,
        offset: u64,
        size: u64,
    ) -> Result<Self, ExtractionModelError> {
        offset
            .checked_add(size)
            .ok_or(ExtractionModelError::SourceRangeOverflow { offset, size })?;
        Ok(Self {
            source,
            offset,
            size,
        })
    }

    #[must_use]
    pub const fn source(&self) -> &SourceLocator {
        &self.source
    }

    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub fn end(&self) -> u64 {
        self.offset + self.size
    }
}

#[derive(Serialize)]
struct ExtractionSourceRangeRef<'value> {
    source: &'value SourceLocator,
    offset: u64,
    size: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractionSourceRangeWire {
    source: SourceLocator,
    offset: u64,
    size: u64,
}

impl Serialize for ExtractionSourceRange {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ExtractionSourceRangeRef {
            source: &self.source,
            offset: self.offset,
            size: self.size,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExtractionSourceRange {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ExtractionSourceRangeWire::deserialize(deserializer)?;
        Self::new(wire.source, wire.offset, wire.size).map_err(serde::de::Error::custom)
    }
}

/// The stable semantic kind of one output artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionArtifactKind {
    BinaryRaw,
    Yaml,
    Text,
    Audio,
    TexturePng,
    SpritePng,
}

impl ExtractionArtifactKind {
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::BinaryRaw => "bin",
            Self::Yaml => "yaml",
            Self::Text => "txt",
            Self::Audio => "audio",
            Self::TexturePng | Self::SpritePng => "png",
        }
    }
}

/// Inert execution data selected during planning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PlannedContent {
    RawBinary,
    Yaml,
    TextAsset,
    Audio {
        #[serde(with = "unity_version_wire")]
        version: UnityVersion,
        extension: String,
        stream: Option<ExtractionSourceRange>,
    },
    TexturePng {
        #[serde(with = "unity_version_wire")]
        version: UnityVersion,
        stream: Option<ExtractionSourceRange>,
    },
    SpritePng {
        #[serde(with = "unity_version_wire")]
        version: UnityVersion,
        texture: ObjectAddress,
        texture_stream: Option<ExtractionSourceRange>,
    },
}

impl PlannedContent {
    fn artifact_kind(&self) -> ExtractionArtifactKind {
        match self {
            Self::RawBinary => ExtractionArtifactKind::BinaryRaw,
            Self::Yaml => ExtractionArtifactKind::Yaml,
            Self::TextAsset => ExtractionArtifactKind::Text,
            Self::Audio { .. } => ExtractionArtifactKind::Audio,
            Self::TexturePng { .. } => ExtractionArtifactKind::TexturePng,
            Self::SpritePng { .. } => ExtractionArtifactKind::SpritePng,
        }
    }

    fn validate(&self) -> Result<(), ExtractionModelError> {
        if let Self::Audio { extension, .. } = self {
            validate_audio_extension(extension)?;
        }
        Ok(())
    }
}

/// Expected identity of one source read by an extraction plan.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionSourceExpectation {
    locator: SourceLocator,
    fingerprint: SourceFingerprint,
}

impl ExtractionSourceExpectation {
    #[must_use]
    pub const fn new(locator: SourceLocator, fingerprint: SourceFingerprint) -> Self {
        Self {
            locator,
            fingerprint,
        }
    }

    #[must_use]
    pub const fn locator(&self) -> &SourceLocator {
        &self.locator
    }

    #[must_use]
    pub const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedFallback {
    kind: ExtractionArtifactKind,
    path: ExtractionPath,
    content: PlannedContent,
}

/// One ordered artifact in an immutable extraction plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedArtifact {
    ordinal: u32,
    address: ObjectAddress,
    class_id: i32,
    class_name: String,
    object_name: Option<String>,
    preferred_kind: ExtractionArtifactKind,
    preferred_path: ExtractionPath,
    preferred_content: PlannedContent,
    fallback: Option<PlannedFallback>,
    working_set_bytes: u64,
    diagnostics: Box<[ExtractionDiagnostic]>,
}

impl PlannedArtifact {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ordinal: u32,
        address: ObjectAddress,
        class_id: i32,
        class_name: String,
        object_name: Option<String>,
        preferred_kind: ExtractionArtifactKind,
        preferred_path: ExtractionPath,
        preferred_content: PlannedContent,
        fallback: Option<(ExtractionArtifactKind, ExtractionPath, PlannedContent)>,
        working_set_bytes: u64,
        diagnostics: Vec<ExtractionDiagnostic>,
    ) -> Result<Self, ExtractionModelError> {
        validate_metadata("class_name", &class_name, false)?;
        if let Some(object_name) = object_name.as_deref() {
            validate_metadata("object_name", object_name, true)?;
        }
        validate_content_kind(preferred_kind, &preferred_content)?;
        let fallback = fallback
            .map(|(kind, path, content)| {
                validate_content_kind(kind, &content)?;
                if preferred_path.portability_key() == path.portability_key() {
                    return Err(ExtractionModelError::FallbackPathCollision(
                        preferred_path.as_str().to_owned(),
                    ));
                }
                Ok(PlannedFallback {
                    kind,
                    path,
                    content,
                })
            })
            .transpose()?;
        if diagnostics
            .iter()
            .any(|diagnostic| diagnostic.address() != Some(&address))
        {
            return Err(ExtractionModelError::InvalidDiagnosticAddress { ordinal });
        }
        let diagnostics = normalize_values(diagnostics).into_boxed_slice();
        Ok(Self {
            ordinal,
            address,
            class_id,
            class_name,
            object_name,
            preferred_kind,
            preferred_path,
            preferred_content,
            fallback,
            working_set_bytes,
            diagnostics,
        })
    }

    #[must_use]
    pub const fn ordinal(&self) -> u32 {
        self.ordinal
    }

    #[must_use]
    pub const fn address(&self) -> &ObjectAddress {
        &self.address
    }

    #[must_use]
    pub const fn class_id(&self) -> i32 {
        self.class_id
    }

    #[must_use]
    pub fn class_name(&self) -> &str {
        &self.class_name
    }

    #[must_use]
    pub fn object_name(&self) -> Option<&str> {
        self.object_name.as_deref()
    }

    #[must_use]
    pub const fn preferred_kind(&self) -> ExtractionArtifactKind {
        self.preferred_kind
    }

    #[must_use]
    pub const fn preferred_path(&self) -> &ExtractionPath {
        &self.preferred_path
    }

    pub(crate) const fn preferred_content(&self) -> &PlannedContent {
        &self.preferred_content
    }

    #[must_use]
    pub fn fallback_kind(&self) -> Option<ExtractionArtifactKind> {
        self.fallback.as_ref().map(|fallback| fallback.kind)
    }

    #[must_use]
    pub fn fallback_path(&self) -> Option<&ExtractionPath> {
        self.fallback.as_ref().map(|fallback| &fallback.path)
    }

    pub(crate) fn fallback_content(&self) -> Option<&PlannedContent> {
        self.fallback.as_ref().map(|fallback| &fallback.content)
    }

    /// Conservative maximum transient bytes retained while encoding this artifact.
    ///
    /// The bound includes staged output so the executor can limit concurrent batches before
    /// creating temporary files.
    #[must_use]
    pub const fn working_set_bytes(&self) -> u64 {
        self.working_set_bytes
    }

    #[must_use]
    pub const fn diagnostics(&self) -> &[ExtractionDiagnostic] {
        &self.diagnostics
    }

    pub(crate) fn matches_output(
        &self,
        kind: ExtractionArtifactKind,
        path: &ExtractionPath,
    ) -> bool {
        (self.preferred_kind == kind && &self.preferred_path == path)
            || self
                .fallback
                .as_ref()
                .is_some_and(|fallback| fallback.kind == kind && &fallback.path == path)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedArtifactWire {
    ordinal: u32,
    address: ObjectAddress,
    class_id: i32,
    class_name: String,
    object_name: Option<String>,
    preferred_kind: ExtractionArtifactKind,
    preferred_path: ExtractionPath,
    preferred_content: PlannedContent,
    fallback: Option<PlannedFallback>,
    working_set_bytes: u64,
    diagnostics: Vec<ExtractionDiagnostic>,
}

impl PlannedArtifactWire {
    fn into_artifact(self) -> Result<PlannedArtifact, ExtractionModelError> {
        PlannedArtifact::new(
            self.ordinal,
            self.address,
            self.class_id,
            self.class_name,
            self.object_name,
            self.preferred_kind,
            self.preferred_path,
            self.preferred_content,
            self.fallback
                .map(|fallback| (fallback.kind, fallback.path, fallback.content)),
            self.working_set_bytes,
            self.diagnostics,
        )
    }
}

/// An immutable, revision-bound extraction plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionPlan {
    workspace_id: WorkspaceId,
    revision: WorkspaceRevision,
    request: ExtractionRequest,
    request_digest: DigestV1,
    sources: Box<[ExtractionSourceExpectation]>,
    artifacts: Box<[PlannedArtifact]>,
}

impl ExtractionPlan {
    pub(crate) fn new(
        workspace_id: WorkspaceId,
        revision: WorkspaceRevision,
        request: ExtractionRequest,
        sources: Vec<ExtractionSourceExpectation>,
        artifacts: Vec<PlannedArtifact>,
    ) -> Result<Self, ExtractionModelError> {
        let request_digest = request.digest()?;
        let sources = normalize_source_expectations(sources)?;
        validate_planned_artifacts(&sources, &artifacts)?;
        Ok(Self {
            workspace_id,
            revision,
            request,
            request_digest,
            sources: sources.into_boxed_slice(),
            artifacts: artifacts.into_boxed_slice(),
        })
    }

    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    #[must_use]
    pub const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub const fn request(&self) -> &ExtractionRequest {
        &self.request
    }

    #[must_use]
    pub const fn request_digest(&self) -> DigestV1 {
        self.request_digest
    }

    #[must_use]
    pub const fn sources(&self) -> &[ExtractionSourceExpectation] {
        &self.sources
    }

    #[must_use]
    pub const fn artifacts(&self) -> &[PlannedArtifact] {
        &self.artifacts
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, ExtractionCanonicalError> {
        canonical_json(self)
    }

    pub fn write_canonical_json(&self, writer: impl Write) -> Result<(), ExtractionCanonicalError> {
        write_canonical_json(writer, self)
    }

    pub fn digest(&self) -> Result<DigestV1, ExtractionCanonicalError> {
        canonical_digest(self)
    }

    pub fn read_json(
        reader: impl Read,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, BudgetedJsonError> {
        read_json_bounded(reader, budget)
    }
}

#[derive(Serialize)]
struct ExtractionPlanRef<'value> {
    contract: &'static str,
    version: u8,
    workspace_id: WorkspaceId,
    revision: WorkspaceRevision,
    request: &'value ExtractionRequest,
    request_digest: DigestV1,
    sources: &'value [ExtractionSourceExpectation],
    artifacts: &'value [PlannedArtifact],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractionPlanWire {
    contract: String,
    version: u8,
    workspace_id: WorkspaceId,
    revision: WorkspaceRevision,
    request: ExtractionRequest,
    request_digest: DigestV1,
    sources: Vec<ExtractionSourceExpectation>,
    artifacts: Vec<PlannedArtifactWire>,
}

impl Serialize for ExtractionPlan {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ExtractionPlanRef {
            contract: EXTRACTION_PLAN_CONTRACT,
            version: EXTRACTION_PLAN_VERSION,
            workspace_id: self.workspace_id,
            revision: self.revision,
            request: &self.request,
            request_digest: self.request_digest,
            sources: &self.sources,
            artifacts: &self.artifacts,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExtractionPlan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ExtractionPlanWire::deserialize(deserializer)?;
        if wire.contract != EXTRACTION_PLAN_CONTRACT {
            return Err(serde::de::Error::custom(
                ExtractionModelError::UnexpectedContract {
                    expected: EXTRACTION_PLAN_CONTRACT,
                    actual: wire.contract,
                },
            ));
        }
        if wire.version != EXTRACTION_PLAN_VERSION {
            return Err(serde::de::Error::custom(
                ExtractionModelError::UnsupportedPlanVersion(wire.version),
            ));
        }
        let artifacts = wire
            .artifacts
            .into_iter()
            .map(PlannedArtifactWire::into_artifact)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::de::Error::custom)?;
        let plan = Self::new(
            wire.workspace_id,
            wire.revision,
            wire.request,
            wire.sources,
            artifacts,
        )
        .map_err(serde::de::Error::custom)?;
        if plan.request_digest != wire.request_digest {
            return Err(serde::de::Error::custom(
                ExtractionModelError::RequestDigestMismatch {
                    declared: wire.request_digest,
                    actual: plan.request_digest,
                },
            ));
        }
        Ok(plan)
    }
}

pub(crate) fn normalize_source_expectations(
    mut sources: Vec<ExtractionSourceExpectation>,
) -> Result<Vec<ExtractionSourceExpectation>, ExtractionModelError> {
    sources.sort_unstable_by(|left, right| left.locator.cmp(&right.locator));
    if let Some(pair) = sources.windows(2).find(|pair| {
        pair[0].locator == pair[1].locator && pair[0].fingerprint != pair[1].fingerprint
    }) {
        return Err(ExtractionModelError::ConflictingSourceExpectation {
            locator: pair[1].locator.clone(),
            first: pair[0].fingerprint,
            second: pair[1].fingerprint,
        });
    }
    sources.dedup_by(|right, left| right.locator == left.locator);
    Ok(sources)
}

fn validate_planned_artifacts(
    sources: &[ExtractionSourceExpectation],
    artifacts: &[PlannedArtifact],
) -> Result<(), ExtractionModelError> {
    let mut addresses = Vec::with_capacity(artifacts.len());
    let mut paths = Vec::with_capacity(artifacts.len().saturating_mul(2));
    for (index, artifact) in artifacts.iter().enumerate() {
        let expected = u32::try_from(index)
            .map_err(|_| ExtractionModelError::ArtifactCountOverflow { count: index + 1 })?;
        if artifact.ordinal != expected {
            return Err(ExtractionModelError::NonCanonicalArtifactOrdinal {
                index,
                expected,
                actual: artifact.ordinal,
            });
        }
        validate_source_for_address(sources, &artifact.address)?;
        validate_content_sources(sources, &artifact.preferred_content)?;
        paths.push(&artifact.preferred_path);
        if let Some(fallback) = artifact.fallback.as_ref() {
            validate_content_sources(sources, &fallback.content)?;
            paths.push(&fallback.path);
        }
        addresses.push(&artifact.address);
    }
    addresses.sort_unstable();
    if let Some(pair) = addresses.windows(2).find(|pair| pair[0] == pair[1]) {
        return Err(ExtractionModelError::DuplicateArtifactAddress(
            (*pair[0]).clone(),
        ));
    }
    validate_unique_paths(paths)
}

fn validate_content_sources(
    sources: &[ExtractionSourceExpectation],
    content: &PlannedContent,
) -> Result<(), ExtractionModelError> {
    match content {
        PlannedContent::RawBinary | PlannedContent::Yaml | PlannedContent::TextAsset => Ok(()),
        PlannedContent::Audio { stream, .. } | PlannedContent::TexturePng { stream, .. } => {
            if let Some(stream) = stream {
                validate_source_exists(sources, stream.source())?;
            }
            Ok(())
        }
        PlannedContent::SpritePng {
            texture,
            texture_stream,
            ..
        } => {
            validate_source_for_address(sources, texture)?;
            if let Some(stream) = texture_stream {
                validate_source_exists(sources, stream.source())?;
            }
            Ok(())
        }
    }
}

fn validate_source_for_address(
    sources: &[ExtractionSourceExpectation],
    address: &ObjectAddress,
) -> Result<(), ExtractionModelError> {
    let source = expectation_for(sources, address.source_locator())?;
    let expected = match address.kind() {
        ObjectKind::Binary => SourceKind::SerializedFile,
        ObjectKind::Yaml => SourceKind::Yaml,
    };
    let actual = source.fingerprint.kind();
    if actual != expected {
        return Err(ExtractionModelError::SourceKindMismatch {
            locator: source.locator.clone(),
            expected,
            actual,
        });
    }
    Ok(())
}

fn validate_source_exists(
    sources: &[ExtractionSourceExpectation],
    locator: &SourceLocator,
) -> Result<(), ExtractionModelError> {
    expectation_for(sources, locator).map(|_| ())
}

fn expectation_for<'source>(
    sources: &'source [ExtractionSourceExpectation],
    locator: &SourceLocator,
) -> Result<&'source ExtractionSourceExpectation, ExtractionModelError> {
    sources
        .binary_search_by(|source| source.locator.cmp(locator))
        .map(|index| &sources[index])
        .map_err(|_| ExtractionModelError::MissingSourceExpectation(locator.clone()))
}

pub(super) fn paths_conflict(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(super) fn first_path_conflict<'path>(
    paths: &mut [&'path ExtractionPath],
) -> Option<(&'path ExtractionPath, &'path ExtractionPath)> {
    paths.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
    if let Some(pair) = paths
        .windows(2)
        .find(|pair| paths_conflict(pair[0].as_str(), pair[1].as_str()))
    {
        return Some((pair[0], pair[1]));
    }
    paths.sort_unstable_by(|left, right| {
        left.portability_key()
            .cmp(right.portability_key())
            .then_with(|| left.as_str().cmp(right.as_str()))
    });
    paths
        .windows(2)
        .find(|pair| paths_conflict(pair[0].portability_key(), pair[1].portability_key()))
        .map(|pair| (pair[0], pair[1]))
}

fn validate_unique_paths(mut paths: Vec<&ExtractionPath>) -> Result<(), ExtractionModelError> {
    if let Some((first, second)) = first_path_conflict(&mut paths) {
        return Err(ExtractionModelError::DuplicateArtifactPath {
            first: first.as_str().to_owned(),
            second: second.as_str().to_owned(),
        });
    }
    Ok(())
}

fn validate_content_kind(
    kind: ExtractionArtifactKind,
    content: &PlannedContent,
) -> Result<(), ExtractionModelError> {
    content.validate()?;
    let actual = content.artifact_kind();
    if kind != actual {
        return Err(ExtractionModelError::ArtifactKindMismatch {
            declared: kind,
            actual,
        });
    }
    Ok(())
}

fn validate_audio_extension(extension: &str) -> Result<(), ExtractionModelError> {
    if extension.is_empty()
        || extension.len() > MAX_AUDIO_EXTENSION_BYTES
        || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(ExtractionModelError::InvalidAudioExtension(
            extension.to_owned(),
        ));
    }
    Ok(())
}

fn validate_metadata(
    field: &'static str,
    value: &str,
    empty_allowed: bool,
) -> Result<(), ExtractionModelError> {
    if (!empty_allowed && value.is_empty())
        || value.len() > MAX_METADATA_TEXT_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ExtractionModelError::InvalidMetadata {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn normalize_selection_pattern(value: String) -> Result<String, ExtractionModelError> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_SELECTION_PATTERN_BYTES
        || trimmed.chars().any(char::is_control)
    {
        return Err(ExtractionModelError::InvalidSelectionPattern(value));
    }
    Ok(trimmed.to_owned())
}

fn normalize_filter_text(
    field: &'static str,
    value: Option<String>,
) -> Result<Option<String>, ExtractionModelError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let normalized: String = trimmed.chars().flat_map(char::to_lowercase).collect();
    if normalized.len() > MAX_FILTER_TEXT_BYTES || normalized.chars().any(char::is_control) {
        return Err(ExtractionModelError::InvalidFilterText { field, value });
    }
    Ok(Some(normalized))
}

pub(super) fn normalize_values<T: Ord>(mut values: Vec<T>) -> Vec<T> {
    values.sort_unstable();
    values.dedup();
    values
}

fn lowercase_contains(value: &str, needle: &str) -> bool {
    value.char_indices().any(|(start, _)| {
        let mut actual = value[start..].chars().flat_map(char::to_lowercase);
        needle
            .chars()
            .all(|expected| actual.next() == Some(expected))
    })
}

mod unity_version_wire {
    use super::*;

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct UnityVersionWire {
        major: u16,
        minor: u16,
        build: u16,
        version_type: UnityVersionType,
        type_number: u8,
        type_str: Option<String>,
    }

    pub(super) fn serialize<S>(value: &UnityVersion, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        UnityVersionWire {
            major: value.major,
            minor: value.minor,
            build: value.build,
            version_type: value.version_type,
            type_number: value.type_number,
            type_str: value.type_str.clone(),
        }
        .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<UnityVersion, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = UnityVersionWire::deserialize(deserializer)?;
        Ok(UnityVersion {
            major: wire.major,
            minor: wire.minor,
            build: wire.build,
            version_type: wire.version_type,
            type_number: wire.type_number,
            type_str: wire.type_str,
        })
    }
}

/// Validation failure for a persisted extraction request or plan.
#[derive(Debug, Error)]
pub enum ExtractionModelError {
    #[error("extraction contract {actual:?} is unsupported; expected {expected:?}")]
    UnexpectedContract {
        expected: &'static str,
        actual: String,
    },
    #[error("extraction request version {0} is unsupported")]
    UnsupportedRequestVersion(u8),
    #[error("extraction plan version {0} is unsupported")]
    UnsupportedPlanVersion(u8),
    #[error("invalid extraction path: {0}")]
    InvalidPath(#[from] ArtifactNameError),
    #[error("bundle container selection pattern is invalid: {0:?}")]
    InvalidSelectionPattern(String),
    #[error("extraction filter {field} is invalid: {value:?}")]
    InvalidFilterText { field: &'static str, value: String },
    #[error("extraction filter limit must be nonzero")]
    ZeroExtractionLimit,
    #[error("source byte range {offset}+{size} overflows u64")]
    SourceRangeOverflow { offset: u64, size: u64 },
    #[error("audio extension must contain 1 to 16 ASCII alphanumeric bytes: {0:?}")]
    InvalidAudioExtension(String),
    #[error("artifact metadata {field} is invalid: {value:?}")]
    InvalidMetadata { field: &'static str, value: String },
    #[error("artifact declares kind {declared:?}, but content requires {actual:?}")]
    ArtifactKindMismatch {
        declared: ExtractionArtifactKind,
        actual: ExtractionArtifactKind,
    },
    #[error("preferred and fallback outputs collide at {0:?}")]
    FallbackPathCollision(String),
    #[error("extraction request digest is {actual}, not declared digest {declared}")]
    RequestDigestMismatch {
        declared: DigestV1,
        actual: DigestV1,
    },
    #[error("source {locator:?} has conflicting fingerprints {first} and {second}")]
    ConflictingSourceExpectation {
        locator: SourceLocator,
        first: SourceFingerprint,
        second: SourceFingerprint,
    },
    #[error("source {0:?} has no expected fingerprint")]
    MissingSourceExpectation(SourceLocator),
    #[error("source {locator:?} has kind {actual:?}; object requires {expected:?}")]
    SourceKindMismatch {
        locator: SourceLocator,
        expected: SourceKind,
        actual: SourceKind,
    },
    #[error("extraction plan contains too many artifacts: {count}")]
    ArtifactCountOverflow { count: usize },
    #[error(
        "planned artifact at index {index} has ordinal {actual}; expected consecutive ordinal {expected}"
    )]
    NonCanonicalArtifactOrdinal {
        index: usize,
        expected: u32,
        actual: u32,
    },
    #[error("extraction plan contains duplicate object address {0:?}")]
    DuplicateArtifactAddress(ObjectAddress),
    #[error("planned artifact {ordinal} contains a diagnostic for another object")]
    InvalidDiagnosticAddress { ordinal: u32 },
    #[error("extraction paths {first:?} and {second:?} cannot coexist on portable filesystems")]
    DuplicateArtifactPath { first: String, second: String },
    #[error(transparent)]
    Canonical(#[from] ExtractionCanonicalError),
}
