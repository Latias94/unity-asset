//! Versioned, deterministic extraction requests and inert plans.

use std::collections::TryReserveError;
use std::io::{Read, Write};
use std::num::NonZeroU64;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedJsonError, DigestV1, ObjectAddress, ObjectKind,
    SourceFingerprint, SourceKind, SourceLocator, WorkspaceId, WorkspaceRevision,
    vec_allocation_bytes,
};
use unity_asset_decode::descriptor::MediaFamily;
use unity_asset_write::artifact::ArtifactNameError;

pub use super::contract::{
    ExtractionArtifactKind, ExtractionDiagnostic, ExtractionPath, ExtractionRepresentationPolicy,
    ExtractionSourceExpectation,
};
use super::json_contract::{large_contract_limits, read_json_bounded, small_contract_limits};
use super::manifest::{
    ExtractionCanonicalError, canonical_digest, canonical_json, write_canonical_json,
};
use super::representation::{
    PlannedContent, PlannedFallback, RepresentationContract, RepresentationContractError,
    RepresentationContractParts,
};

pub const EXTRACTION_REQUEST_VERSION: u8 = 1;
pub const EXTRACTION_PLAN_VERSION: u8 = 3;
pub const EXTRACTION_MANIFEST_VERSION: u8 = super::manifest::EXTRACTION_MANIFEST_VERSION;
pub const EXTRACTION_REPORT_VERSION: u8 = super::manifest::EXTRACTION_REPORT_VERSION;
pub const EXTRACTION_REQUEST_CONTRACT: &str = "unity_asset.extraction_request";
pub const EXTRACTION_PLAN_CONTRACT: &str = "unity_asset.extraction_plan";

const EXTRACTION_REQUEST_JSON_LIMITS: unity_asset_core::ContractJsonLimits =
    small_contract_limits(EXTRACTION_REQUEST_CONTRACT);
const EXTRACTION_PLAN_JSON_LIMITS: unity_asset_core::ContractJsonLimits =
    large_contract_limits(EXTRACTION_PLAN_CONTRACT);

const MAX_SELECTION_PATTERN_BYTES: usize = 4_096;
const MAX_FILTER_TEXT_BYTES: usize = 4_096;
const MAX_METADATA_TEXT_BYTES: usize = 64 * 1_024;

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
        read_json_bounded(reader, budget, EXTRACTION_REQUEST_JSON_LIMITS)
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

/// One ordered artifact in an immutable extraction plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedArtifact {
    ordinal: u32,
    address: ObjectAddress,
    class_id: i32,
    class_name: String,
    object_name: Option<String>,
    representation: RepresentationContract,
}

impl PlannedArtifact {
    pub(super) fn new(
        ordinal: u32,
        address: ObjectAddress,
        class_id: i32,
        class_name: String,
        object_name: Option<String>,
        representation: RepresentationContract,
    ) -> Result<Self, ExtractionModelError> {
        validate_metadata("class_name", &class_name, false)?;
        if let Some(object_name) = object_name.as_deref() {
            validate_metadata("object_name", object_name, true)?;
        }
        Ok(Self {
            ordinal,
            address,
            class_id,
            class_name,
            object_name,
            representation,
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
        self.representation.preferred_kind()
    }

    #[must_use]
    pub const fn preferred_path(&self) -> &ExtractionPath {
        self.representation.preferred_path()
    }

    pub(super) const fn preferred_content(&self) -> &PlannedContent {
        self.representation.preferred_content()
    }

    pub(super) const fn preferred_requires_write_budget(&self) -> bool {
        self.representation.preferred_requires_write_budget()
    }

    #[must_use]
    pub fn fallback_kind(&self) -> Option<ExtractionArtifactKind> {
        self.representation.fallback_kind()
    }

    #[must_use]
    pub fn fallback_path(&self) -> Option<&ExtractionPath> {
        self.representation.fallback_path()
    }

    /// Planner-declared conservative maximum transient bytes for this artifact.
    ///
    /// The executor derives an authoritative bound from the exact workspace revision before it
    /// creates output and rejects declarations below that proof.
    #[must_use]
    pub const fn working_set_bytes(&self) -> u64 {
        self.representation.working_set_bytes()
    }

    #[must_use]
    pub const fn diagnostics(&self) -> &[ExtractionDiagnostic] {
        self.representation.diagnostics()
    }

    pub(crate) fn matches_output(
        &self,
        kind: ExtractionArtifactKind,
        path: &ExtractionPath,
    ) -> bool {
        self.representation.matches_output(kind, path)
    }

    pub(super) const fn representation(&self) -> &RepresentationContract {
        &self.representation
    }
}

#[derive(Serialize)]
struct PlannedFallbackRef<'artifact> {
    kind: ExtractionArtifactKind,
    path: &'artifact ExtractionPath,
    content: &'artifact PlannedContent,
}

impl<'artifact> From<&'artifact PlannedFallback> for PlannedFallbackRef<'artifact> {
    fn from(fallback: &'artifact PlannedFallback) -> Self {
        Self {
            kind: fallback.kind(),
            path: fallback.path(),
            content: fallback.content(),
        }
    }
}

#[derive(Serialize)]
struct PlannedArtifactRef<'artifact> {
    ordinal: u32,
    address: &'artifact ObjectAddress,
    class_id: i32,
    class_name: &'artifact str,
    object_name: Option<&'artifact str>,
    preferred_kind: ExtractionArtifactKind,
    preferred_path: &'artifact ExtractionPath,
    preferred_content: &'artifact PlannedContent,
    fallback: Option<PlannedFallbackRef<'artifact>>,
    working_set_bytes: u64,
    diagnostics: &'artifact [ExtractionDiagnostic],
}

impl Serialize for PlannedArtifact {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        PlannedArtifactRef {
            ordinal: self.ordinal,
            address: &self.address,
            class_id: self.class_id,
            class_name: &self.class_name,
            object_name: self.object_name.as_deref(),
            preferred_kind: self.preferred_kind(),
            preferred_path: self.preferred_path(),
            preferred_content: self.preferred_content(),
            fallback: self.representation.fallback().map(PlannedFallbackRef::from),
            working_set_bytes: self.working_set_bytes(),
            diagnostics: self.diagnostics(),
        }
        .serialize(serializer)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedFallbackWire {
    kind: ExtractionArtifactKind,
    path: ExtractionPath,
    content: PlannedContent,
}

impl PlannedFallbackWire {
    fn into_fallback(self) -> Result<PlannedFallback, ExtractionModelError> {
        PlannedFallback::from_declared_parts(self.kind, self.path, self.content).map_err(Into::into)
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
    fallback: Option<PlannedFallbackWire>,
    working_set_bytes: u64,
    diagnostics: Vec<ExtractionDiagnostic>,
}

impl PlannedArtifactWire {
    fn into_artifact(self) -> Result<PlannedArtifact, ExtractionModelError> {
        let Self {
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
        } = self;
        preferred_content.validate_declared_kind(preferred_kind)?;
        let fallback = fallback
            .map(PlannedFallbackWire::into_fallback)
            .transpose()?;
        let representation = RepresentationContract::from_parts(
            ordinal,
            &address,
            RepresentationContractParts {
                preferred_path,
                preferred_content,
                fallback,
                working_set_bytes,
                diagnostics,
            },
        )?;
        PlannedArtifact::new(
            ordinal,
            address,
            class_id,
            class_name,
            object_name,
            representation,
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

    pub(crate) fn new_budgeted(
        workspace_id: WorkspaceId,
        revision: WorkspaceRevision,
        request: ExtractionRequest,
        sources: Vec<ExtractionSourceExpectation>,
        artifacts: Vec<PlannedArtifact>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ExtractionModelError> {
        let request_digest = request.digest()?;
        let sources = normalize_source_expectations(sources)?;
        validate_planned_artifacts_budgeted(&sources, &artifacts, budget)?;
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
        read_json_bounded(reader, budget, EXTRACTION_PLAN_JSON_LIMITS)
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
    sources.sort_unstable_by(|left, right| left.locator().cmp(right.locator()));
    if let Some(pair) = sources.windows(2).find(|pair| {
        pair[0].locator() == pair[1].locator() && pair[0].fingerprint() != pair[1].fingerprint()
    }) {
        return Err(ExtractionModelError::ConflictingSourceExpectation {
            locator: pair[1].locator().clone(),
            first: pair[0].fingerprint(),
            second: pair[1].fingerprint(),
        });
    }
    sources.dedup_by(|right, left| right.locator() == left.locator());
    Ok(sources)
}

fn validate_planned_artifacts(
    sources: &[ExtractionSourceExpectation],
    artifacts: &[PlannedArtifact],
) -> Result<(), ExtractionModelError> {
    validate_planned_artifacts_with_budget(sources, artifacts, None)
}

fn validate_planned_artifacts_budgeted(
    sources: &[ExtractionSourceExpectation],
    artifacts: &[PlannedArtifact],
    budget: &mut AssetLoadBudget,
) -> Result<(), ExtractionModelError> {
    validate_planned_artifacts_with_budget(sources, artifacts, Some(budget))
}

fn validate_planned_artifacts_with_budget(
    sources: &[ExtractionSourceExpectation],
    artifacts: &[PlannedArtifact],
    mut budget: Option<&mut AssetLoadBudget>,
) -> Result<(), ExtractionModelError> {
    let path_capacity =
        artifacts
            .len()
            .checked_mul(2)
            .ok_or(ExtractionModelError::ArithmeticOverflow {
                resource: "extraction plan validation paths",
            })?;
    let mut addresses = validation_vec(
        artifacts.len(),
        "extraction plan validation addresses",
        reborrow_budget(&mut budget),
    )?;
    let mut paths = validation_vec(
        path_capacity,
        "extraction plan validation paths",
        reborrow_budget(&mut budget),
    )?;
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
        artifact.representation.validate_sources(sources)?;
        paths.push(artifact.preferred_path());
        if let Some(path) = artifact.fallback_path() {
            paths.push(path);
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

fn reborrow_budget<'borrow>(
    budget: &'borrow mut Option<&mut AssetLoadBudget>,
) -> Option<&'borrow mut AssetLoadBudget> {
    match budget {
        Some(budget) => Some(&mut **budget),
        None => None,
    }
}

fn validation_vec<T>(
    capacity: usize,
    resource: &'static str,
    budget: Option<&mut AssetLoadBudget>,
) -> Result<Vec<T>, ExtractionModelError> {
    let entries =
        u64::try_from(capacity).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    let minimum_bytes = vec_allocation_bytes::<T>(capacity)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    if let Some(budget) = budget.as_deref() {
        budget.check_entries(entries)?;
        budget.check_bytes(minimum_bytes)?;
    }

    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|source| ExtractionModelError::Allocation {
            resource,
            requested: capacity,
            source,
        })?;
    if let Some(budget) = budget {
        let retained_bytes = vec_allocation_bytes::<T>(values.capacity())
            .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
        budget.check_bytes(retained_bytes)?;
        budget.consume_entries(entries)?;
        budget.consume_bytes(retained_bytes)?;
    }
    Ok(values)
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
    let actual = source.fingerprint().kind();
    if actual != expected {
        return Err(ExtractionModelError::SourceKindMismatch {
            locator: source.locator().clone(),
            expected,
            actual,
        });
    }
    Ok(())
}

fn expectation_for<'source>(
    sources: &'source [ExtractionSourceExpectation],
    locator: &SourceLocator,
) -> Result<&'source ExtractionSourceExpectation, ExtractionModelError> {
    sources
        .binary_search_by(|source| source.locator().cmp(locator))
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

/// Validation failure for a persisted extraction request or plan.
#[non_exhaustive]
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
    #[error("media descriptor family is {actual:?}; representation requires {expected:?}")]
    MediaDescriptorFamilyMismatch {
        expected: MediaFamily,
        actual: MediaFamily,
    },
    #[error("artifact path {path:?} must end in the canonical .{expected} suffix")]
    ArtifactExtensionMismatch {
        path: String,
        expected: &'static str,
    },
    #[error("artifact metadata {field} is invalid: {value:?}")]
    InvalidMetadata { field: &'static str, value: String },
    #[error("artifact declares kind {declared:?}, but content requires {actual:?}")]
    ArtifactKindMismatch {
        declared: ExtractionArtifactKind,
        actual: ExtractionArtifactKind,
    },
    #[error("preferred and fallback outputs collide at {0:?}")]
    FallbackPathCollision(String),
    #[error("decoded extraction fallbacks must be raw binary outputs")]
    InvalidFallbackContent,
    #[error("planned artifact {ordinal} declares a zero-byte working set")]
    ZeroWorkingSet { ordinal: u32 },
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
    #[error("arithmetic overflow while validating {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("failed to reserve {requested} capacity units for {resource}: {source}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Canonical(#[from] ExtractionCanonicalError),
}

impl From<RepresentationContractError> for ExtractionModelError {
    fn from(error: RepresentationContractError) -> Self {
        match error {
            RepresentationContractError::MediaDescriptorFamilyMismatch { expected, actual } => {
                Self::MediaDescriptorFamilyMismatch { expected, actual }
            }
            RepresentationContractError::ArtifactExtensionMismatch { path, expected } => {
                Self::ArtifactExtensionMismatch { path, expected }
            }
            RepresentationContractError::ArtifactKindMismatch { declared, actual } => {
                Self::ArtifactKindMismatch { declared, actual }
            }
            RepresentationContractError::FallbackPathCollision(path) => {
                Self::FallbackPathCollision(path)
            }
            RepresentationContractError::InvalidFallbackContent => Self::InvalidFallbackContent,
            RepresentationContractError::ZeroWorkingSet { ordinal } => {
                Self::ZeroWorkingSet { ordinal }
            }
            RepresentationContractError::InvalidDiagnosticAddress { ordinal } => {
                Self::InvalidDiagnosticAddress { ordinal }
            }
            RepresentationContractError::MissingSourceExpectation(locator) => {
                Self::MissingSourceExpectation(locator)
            }
            RepresentationContractError::SourceKindMismatch {
                locator,
                expected,
                actual,
            } => Self::SourceKindMismatch {
                locator,
                expected,
                actual,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned_text_artifact() -> PlannedArtifact {
        let address =
            ObjectAddress::binary_direct(SourceLocator::path("content.assets").unwrap(), 41)
                .unwrap();
        let fallback = PlannedFallback::new(
            ExtractionPath::new("object.raw.bin").unwrap(),
            PlannedContent::RawBinary,
        )
        .unwrap();
        let representation = RepresentationContract::from_parts(
            0,
            &address,
            RepresentationContractParts {
                preferred_path: ExtractionPath::new("object.txt").unwrap(),
                preferred_content: PlannedContent::TextAsset,
                fallback: Some(fallback),
                working_set_bytes: 1,
                diagnostics: Vec::new(),
            },
        )
        .unwrap();
        PlannedArtifact::new(
            0,
            address,
            49,
            "TextAsset".to_owned(),
            Some("object".to_owned()),
            representation,
        )
        .unwrap()
    }

    #[test]
    fn plan_v3_artifact_serializes_derived_kinds_in_the_existing_wire_shape() {
        let artifact = planned_text_artifact();
        let encoded = serde_json::to_value(&artifact).unwrap();

        assert_eq!(encoded["preferred_kind"], serde_json::json!("text"));
        assert_eq!(
            encoded["preferred_content"]["kind"],
            serde_json::json!("text_asset")
        );
        assert_eq!(encoded["fallback"]["kind"], serde_json::json!("binary_raw"));
        assert_eq!(
            encoded["fallback"]["content"]["kind"],
            serde_json::json!("raw_binary")
        );

        let wire: PlannedArtifactWire = serde_json::from_value(encoded).unwrap();
        assert_eq!(wire.into_artifact().unwrap(), artifact);
    }

    #[test]
    fn plan_v3_artifact_rejects_tampered_preferred_and_fallback_kinds() {
        let encoded = serde_json::to_value(planned_text_artifact()).unwrap();

        let mut preferred = encoded.clone();
        preferred["preferred_kind"] = serde_json::json!("yaml");
        let error = serde_json::from_value::<PlannedArtifactWire>(preferred)
            .unwrap()
            .into_artifact()
            .unwrap_err();
        assert!(matches!(
            error,
            ExtractionModelError::ArtifactKindMismatch {
                declared: ExtractionArtifactKind::Yaml,
                actual: ExtractionArtifactKind::Text,
            }
        ));

        let mut fallback = encoded;
        fallback["fallback"]["kind"] = serde_json::json!("text");
        let error = serde_json::from_value::<PlannedArtifactWire>(fallback)
            .unwrap()
            .into_artifact()
            .unwrap_err();
        assert!(matches!(
            error,
            ExtractionModelError::ArtifactKindMismatch {
                declared: ExtractionArtifactKind::Text,
                actual: ExtractionArtifactKind::BinaryRaw,
            }
        ));
    }
}
