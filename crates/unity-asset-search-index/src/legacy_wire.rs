//! Frozen storage-v1 wire types that predate canonical YAML file IDs.
//!
//! Current domain types must not deserialize these payloads directly. In particular, the v1
//! object-address contract accepted arbitrary validated YAML anchor spellings, while the current
//! contract accepts only canonical, nonzero Unity `fileID` values. This module preserves the old
//! bytes for validation and makes every lossy boundary explicit through a typed conversion error.

use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ContractError, Diagnostic, DiagnosticError, DiagnosticSeverity,
    DigestBuildError, DigestV1, DigestV1Builder, FieldPath, ObjectAddress, SourceLocator,
    WorkspaceId, WorkspaceRevision, YamlFileId,
};
use unity_asset_search_protocol::MAX_PORTABLE_PATH_BYTES;

use crate::analysis::{
    AnalysisTruncation, AnalyzedSource, ContainerEntryFact, RawReferenceProjection,
    ReferenceDependencyKey, ReferenceProjectionFact, ReferenceResolutionProjection, SearchFacts,
};
use crate::generation_store::{SourceScanHint, TransactionReceiptWindow};

const LEGACY_WIRE_VERSION: u16 = 1;
const MAX_YAML_ANCHOR_BYTES: usize = 1024;
const MAX_DIAGNOSTIC_CODE_BYTES: usize = 128;
const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_SOURCE_STATE_ASSETS: usize = 1_000_000;
const MAX_SOURCE_STATE_SCAN_HINTS: usize = 1_000_000;

/// Failure while validating or converting a frozen storage-v1 wire value.
#[derive(Debug)]
pub(crate) enum LegacyWireError {
    UnsupportedVersion {
        contract: &'static str,
        actual: u16,
        expected: u16,
    },
    NullBinaryPathId,
    DirectAddressContainsBundleMember,
    BundleAddressMissingMember,
    InvalidYamlAnchor(String),
    UnrepresentableYamlAnchor(String),
    DiagnosticCodeTooLong,
    InvalidDiagnosticCode(String),
    DiagnosticMessageTooLong,
    EmptyDiagnosticMessage,
    AddressConversion(ContractError),
    DiagnosticConversion(DiagnosticError),
    Budget(BudgetError),
    AllocationFailed {
        resource: &'static str,
        requested: usize,
    },
    EmptyStableId,
    StableIdMismatch,
    SourceIdentityMismatch,
    CollectionTooLarge {
        collection: &'static str,
        actual: usize,
        maximum: usize,
    },
    NonCanonicalOrder {
        collection: &'static str,
    },
    NonCanonicalAnalysis {
        relative_path: String,
    },
    InvalidRelativePath {
        relative_path: String,
        maximum_bytes: usize,
    },
    InvalidTransactionReceipts {
        message: String,
    },
    LogicalDigestMismatch {
        expected: DigestV1,
        actual: DigestV1,
    },
    SizeOverflow {
        resource: &'static str,
    },
    Json(serde_json::Error),
    Digest(DigestBuildError),
}

impl fmt::Display for LegacyWireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion {
                contract,
                actual,
                expected,
            } => write!(
                formatter,
                "legacy {contract} version {actual} does not match {expected}"
            ),
            Self::NullBinaryPathId => {
                formatter.write_str("legacy binary pathID zero cannot identify an object")
            }
            Self::DirectAddressContainsBundleMember => formatter.write_str(
                "legacy direct binary address contains a bundle member containment step",
            ),
            Self::BundleAddressMissingMember => formatter.write_str(
                "legacy bundle-member binary address has no bundle member containment step",
            ),
            Self::InvalidYamlAnchor(anchor) => {
                write!(formatter, "legacy YAML anchor {anchor:?} is invalid")
            }
            Self::UnrepresentableYamlAnchor(anchor) => write!(
                formatter,
                "legacy YAML anchor {anchor:?} is not a canonical nonzero i64 fileID"
            ),
            Self::DiagnosticCodeTooLong => {
                formatter.write_str("legacy diagnostic code exceeds its maximum encoded length")
            }
            Self::InvalidDiagnosticCode(code) => write!(
                formatter,
                "legacy diagnostic code {code:?} contains unsupported characters"
            ),
            Self::DiagnosticMessageTooLong => {
                formatter.write_str("legacy diagnostic message exceeds its maximum encoded length")
            }
            Self::EmptyDiagnosticMessage => {
                formatter.write_str("legacy diagnostic message must not be empty")
            }
            Self::AddressConversion(error) => {
                write!(
                    formatter,
                    "legacy object address cannot be converted: {error}"
                )
            }
            Self::DiagnosticConversion(error) => {
                write!(formatter, "legacy diagnostic cannot be converted: {error}")
            }
            Self::Budget(error) => fmt::Display::fmt(error, formatter),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "failed to reserve {requested} entries for legacy {resource}"
            ),
            Self::EmptyStableId => {
                formatter.write_str("legacy reference payload stable ID is empty")
            }
            Self::StableIdMismatch => formatter
                .write_str("legacy reference payload stable ID differs from the indexed stable ID"),
            Self::SourceIdentityMismatch => formatter.write_str(
                "legacy reference payload source identity differs from its reference fact",
            ),
            Self::CollectionTooLarge {
                collection,
                actual,
                maximum,
            } => write!(
                formatter,
                "legacy {collection} contains {actual} entries; maximum is {maximum}"
            ),
            Self::NonCanonicalOrder { collection } => {
                write!(formatter, "legacy {collection} is not sorted and unique")
            }
            Self::NonCanonicalAnalysis { relative_path } => write!(
                formatter,
                "legacy analysis for {relative_path:?} is not canonical"
            ),
            Self::InvalidRelativePath {
                relative_path,
                maximum_bytes,
            } => write!(
                formatter,
                "legacy source-state path {relative_path:?} is not a portable relative path of at most {maximum_bytes} bytes"
            ),
            Self::InvalidTransactionReceipts { message } => {
                write!(
                    formatter,
                    "legacy transaction receipts are invalid: {message}"
                )
            }
            Self::LogicalDigestMismatch { expected, actual } => write!(
                formatter,
                "legacy source-state logical digest {actual} does not match persisted digest {expected}"
            ),
            Self::SizeOverflow { resource } => {
                write!(formatter, "legacy {resource} size overflowed")
            }
            Self::Json(error) => write!(formatter, "legacy canonical JSON failed: {error}"),
            Self::Digest(error) => write!(formatter, "legacy canonical digest failed: {error}"),
        }
    }
}

impl Error for LegacyWireError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AddressConversion(error) => Some(error),
            Self::DiagnosticConversion(error) => Some(error),
            Self::Budget(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Digest(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LegacyYamlDocumentSelectorV1 {
    Anchored { anchor: String },
    Unanchored { document_index: u32 },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum LegacyYamlDocumentSelectorRefV1<'value> {
    Anchored { anchor: &'value str },
    Unanchored { document_index: u32 },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum LegacyYamlDocumentSelectorWireV1 {
    Anchored { anchor: String },
    Unanchored { document_index: u32 },
}

impl Serialize for LegacyYamlDocumentSelectorV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Anchored { anchor } => LegacyYamlDocumentSelectorRefV1::Anchored {
                anchor: anchor.as_str(),
            },
            Self::Unanchored { document_index } => LegacyYamlDocumentSelectorRefV1::Unanchored {
                document_index: *document_index,
            },
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LegacyYamlDocumentSelectorV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match LegacyYamlDocumentSelectorWireV1::deserialize(deserializer)? {
            LegacyYamlDocumentSelectorWireV1::Anchored { anchor } => {
                validate_legacy_yaml_anchor(&anchor).map_err(serde::de::Error::custom)?;
                Ok(Self::Anchored { anchor })
            }
            LegacyYamlDocumentSelectorWireV1::Unanchored { document_index } => {
                Ok(Self::Unanchored { document_index })
            }
        }
    }
}

/// Exact storage-v1 `ObjectAddress` wire contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LegacyObjectAddressV1 {
    BinaryDirect {
        source: SourceLocator,
        path_id: i64,
    },
    BinaryBundleMember {
        source: SourceLocator,
        path_id: i64,
    },
    Yaml {
        source: SourceLocator,
        selector: LegacyYamlDocumentSelectorV1,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum LegacyObjectAddressRefV1<'value> {
    BinaryDirect {
        version: u8,
        source: &'value SourceLocator,
        path_id: i64,
    },
    BinaryBundleMember {
        version: u8,
        source: &'value SourceLocator,
        path_id: i64,
    },
    Yaml {
        version: u8,
        source: &'value SourceLocator,
        selector: &'value LegacyYamlDocumentSelectorV1,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum LegacyObjectAddressWireV1 {
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
        selector: LegacyYamlDocumentSelectorV1,
    },
}

impl LegacyObjectAddressV1 {
    #[must_use]
    pub(crate) const fn source_locator(&self) -> &SourceLocator {
        match self {
            Self::BinaryDirect { source, .. }
            | Self::BinaryBundleMember { source, .. }
            | Self::Yaml { source, .. } => source,
        }
    }

    pub(crate) fn try_into_current(self) -> Result<ObjectAddress, LegacyWireError> {
        match self {
            Self::BinaryDirect { source, path_id } => ObjectAddress::binary_direct(source, path_id)
                .map_err(LegacyWireError::AddressConversion),
            Self::BinaryBundleMember { source, path_id } => {
                if source.bundle_member().is_none() {
                    return Err(LegacyWireError::BundleAddressMissingMember);
                }
                ObjectAddress::binary_at(source, path_id)
                    .map_err(LegacyWireError::AddressConversion)
            }
            Self::Yaml { source, selector } => match selector {
                LegacyYamlDocumentSelectorV1::Anchored { anchor } => {
                    validate_legacy_yaml_anchor(&anchor)?;
                    let file_id = YamlFileId::parse_canonical(&anchor)
                        .map_err(|_| LegacyWireError::UnrepresentableYamlAnchor(anchor))?;
                    ObjectAddress::yaml(source, file_id).map_err(LegacyWireError::AddressConversion)
                }
                LegacyYamlDocumentSelectorV1::Unanchored { document_index } => {
                    ObjectAddress::yaml_document(source, document_index)
                        .map_err(LegacyWireError::AddressConversion)
                }
            },
        }
    }

    fn semantic_key(&self) -> LegacyAddressSemanticKey<'_> {
        match self {
            Self::BinaryDirect { path_id, .. } | Self::BinaryBundleMember { path_id, .. } => {
                LegacyAddressSemanticKey::Binary(*path_id)
            }
            Self::Yaml { selector, .. } => LegacyAddressSemanticKey::Yaml(selector),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LegacyAddressSemanticKey<'value> {
    Binary(i64),
    Yaml(&'value LegacyYamlDocumentSelectorV1),
}

impl PartialOrd for LegacyObjectAddressV1 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LegacyObjectAddressV1 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.source_locator()
            .cmp(other.source_locator())
            .then_with(|| self.semantic_key().cmp(&other.semantic_key()))
    }
}

impl Serialize for LegacyObjectAddressV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::BinaryDirect { source, path_id } => LegacyObjectAddressRefV1::BinaryDirect {
                version: LEGACY_WIRE_VERSION as u8,
                source,
                path_id: *path_id,
            },
            Self::BinaryBundleMember { source, path_id } => {
                LegacyObjectAddressRefV1::BinaryBundleMember {
                    version: LEGACY_WIRE_VERSION as u8,
                    source,
                    path_id: *path_id,
                }
            }
            Self::Yaml { source, selector } => LegacyObjectAddressRefV1::Yaml {
                version: LEGACY_WIRE_VERSION as u8,
                source,
                selector,
            },
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LegacyObjectAddressV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = LegacyObjectAddressWireV1::deserialize(deserializer)?;
        let address = match wire {
            LegacyObjectAddressWireV1::BinaryDirect {
                version,
                source,
                path_id,
            } => {
                validate_version("object address", version.into())
                    .map_err(serde::de::Error::custom)?;
                if path_id == 0 {
                    return Err(serde::de::Error::custom(LegacyWireError::NullBinaryPathId));
                }
                if source.bundle_member().is_some() {
                    return Err(serde::de::Error::custom(
                        LegacyWireError::DirectAddressContainsBundleMember,
                    ));
                }
                Self::BinaryDirect { source, path_id }
            }
            LegacyObjectAddressWireV1::BinaryBundleMember {
                version,
                source,
                path_id,
            } => {
                validate_version("object address", version.into())
                    .map_err(serde::de::Error::custom)?;
                if path_id == 0 {
                    return Err(serde::de::Error::custom(LegacyWireError::NullBinaryPathId));
                }
                if source.bundle_member().is_none() {
                    return Err(serde::de::Error::custom(
                        LegacyWireError::BundleAddressMissingMember,
                    ));
                }
                Self::BinaryBundleMember { source, path_id }
            }
            LegacyObjectAddressWireV1::Yaml {
                version,
                source,
                selector,
            } => {
                validate_version("object address", version.into())
                    .map_err(serde::de::Error::custom)?;
                Self::Yaml { source, selector }
            }
        };
        Ok(address)
    }
}

/// Exact storage-v1 diagnostic wire contract.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LegacyDiagnosticV1 {
    severity: DiagnosticSeverity,
    code: String,
    message: String,
    address: Option<LegacyObjectAddressV1>,
    field_path: Option<FieldPath>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct LegacyDiagnosticRefV1<'value> {
    version: u8,
    severity: DiagnosticSeverity,
    code: &'value str,
    message: &'value str,
    address: &'value Option<LegacyObjectAddressV1>,
    field_path: &'value Option<FieldPath>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyDiagnosticWireV1 {
    version: u8,
    severity: DiagnosticSeverity,
    code: String,
    message: String,
    address: Option<LegacyObjectAddressV1>,
    field_path: Option<FieldPath>,
}

impl LegacyDiagnosticV1 {
    fn try_into_current_with_degradation(
        self,
        degradations: &mut Vec<LegacyAddressDegradation>,
    ) -> Result<Diagnostic, LegacyWireError> {
        let mut diagnostic = Diagnostic::new(self.severity, self.code, self.message)
            .map_err(LegacyWireError::DiagnosticConversion)?;
        if let Some(address) = self.address
            && let Some(address) =
                convert_address_or_degrade(address, "diagnostic address", degradations)?
        {
            diagnostic = diagnostic.at_address(address);
        }
        if let Some(field_path) = self.field_path {
            diagnostic = diagnostic.at_field(field_path);
        }
        Ok(diagnostic)
    }
}

impl Serialize for LegacyDiagnosticV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        LegacyDiagnosticRefV1 {
            version: LEGACY_WIRE_VERSION as u8,
            severity: self.severity,
            code: &self.code,
            message: &self.message,
            address: &self.address,
            field_path: &self.field_path,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LegacyDiagnosticV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = LegacyDiagnosticWireV1::deserialize(deserializer)?;
        validate_version("diagnostic", wire.version.into()).map_err(serde::de::Error::custom)?;
        validate_diagnostic_text(&wire.code, &wire.message).map_err(serde::de::Error::custom)?;
        Ok(Self {
            severity: wire.severity,
            code: wire.code,
            message: wire.message,
            address: wire.address,
            field_path: wire.field_path,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyWorkspaceGraphInputsV1 {
    #[serde(default)]
    complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    objects: Vec<LegacyWorkspaceObjectFactV1>,
}

impl LegacyWorkspaceGraphInputsV1 {
    fn is_empty(&self) -> bool {
        !self.complete && self.objects.is_empty()
    }

    fn validate_canonical_order(&self) -> bool {
        self.objects
            .windows(2)
            .all(|pair| pair[0].address < pair[1].address)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyWorkspaceObjectFactV1 {
    address: LegacyObjectAddressV1,
    class_id: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum LegacyReferenceResolutionProjectionV1 {
    Null,
    Resolved {
        target: LegacyObjectAddressV1,
    },
    Unloaded {
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<SourceLocator>,
    },
    Missing {
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<LegacyObjectAddressV1>,
    },
    Ambiguous {
        candidates: Vec<LegacyObjectAddressV1>,
    },
    Invalid,
}

impl LegacyReferenceResolutionProjectionV1 {
    fn candidate_count(&self) -> usize {
        match self {
            Self::Ambiguous { candidates } => candidates.len(),
            _ => 0,
        }
    }

    fn address_count(&self) -> usize {
        match self {
            Self::Resolved { .. } | Self::Missing { target: Some(_) } => 1,
            Self::Ambiguous { candidates } => candidates.len(),
            Self::Null | Self::Unloaded { .. } | Self::Missing { target: None } | Self::Invalid => {
                0
            }
        }
    }

    fn try_into_current_with_degradation(
        self,
        degradations: &mut Vec<LegacyAddressDegradation>,
    ) -> Result<ReferenceResolutionProjection, LegacyWireError> {
        match self {
            Self::Null => Ok(ReferenceResolutionProjection::Null),
            Self::Resolved { target } => {
                match convert_address_or_degrade(target, "resolved target", degradations)? {
                    Some(target) => Ok(ReferenceResolutionProjection::Resolved { target }),
                    None => Ok(ReferenceResolutionProjection::Invalid),
                }
            }
            Self::Unloaded { source } => Ok(ReferenceResolutionProjection::Unloaded { source }),
            Self::Missing { target: None } => {
                Ok(ReferenceResolutionProjection::Missing { target: None })
            }
            Self::Missing {
                target: Some(target),
            } => match convert_address_or_degrade(target, "missing target", degradations)? {
                Some(target) => Ok(ReferenceResolutionProjection::Missing {
                    target: Some(target),
                }),
                None => Ok(ReferenceResolutionProjection::Invalid),
            },
            Self::Ambiguous { candidates } => {
                let mut converted = Vec::new();
                converted.try_reserve_exact(candidates.len()).map_err(|_| {
                    LegacyWireError::AllocationFailed {
                        resource: "ambiguous reference candidates",
                        requested: candidates.len(),
                    }
                })?;
                let mut complete = true;
                for candidate in candidates {
                    match convert_address_or_degrade(candidate, "ambiguous target", degradations)? {
                        Some(candidate) => converted.push(candidate),
                        None => complete = false,
                    }
                }
                if complete {
                    Ok(ReferenceResolutionProjection::Ambiguous {
                        candidates: converted,
                    })
                } else {
                    Ok(ReferenceResolutionProjection::Invalid)
                }
            }
            Self::Invalid => Ok(ReferenceResolutionProjection::Invalid),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum LegacyReferenceDependencyKeyV1 {
    Guid {
        guid: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_id: Option<i64>,
    },
    Object {
        address: LegacyObjectAddressV1,
    },
    Source {
        locator: SourceLocator,
    },
}

impl LegacyReferenceDependencyKeyV1 {
    fn try_into_current_with_degradation(
        self,
        degradations: &mut Vec<LegacyAddressDegradation>,
    ) -> Result<Option<ReferenceDependencyKey>, LegacyWireError> {
        match self {
            Self::Guid { guid, file_id } => {
                Ok(Some(ReferenceDependencyKey::Guid { guid, file_id }))
            }
            Self::Object { address } => {
                Ok(
                    convert_address_or_degrade(address, "object dependency", degradations)?
                        .map(|address| ReferenceDependencyKey::Object { address }),
                )
            }
            Self::Source { locator } => Ok(Some(ReferenceDependencyKey::Source { locator })),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyReferenceProjectionFactV1 {
    #[serde(skip_serializing_if = "Option::is_none")]
    source_object: Option<LegacyObjectAddressV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_file_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_class_id: Option<i32>,
    field_path: FieldPath,
    raw_target: RawReferenceProjection,
    resolution: LegacyReferenceResolutionProjectionV1,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    diagnostics: Vec<LegacyDiagnosticV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    dependency_keys: Vec<LegacyReferenceDependencyKeyV1>,
}

impl LegacyReferenceProjectionFactV1 {
    fn validate_canonical_order(&self) -> bool {
        is_strictly_sorted(&self.diagnostics) && is_strictly_sorted(&self.dependency_keys)
    }

    fn nested_collection_count(&self) -> Result<usize, LegacyWireError> {
        self.diagnostics
            .len()
            .checked_add(self.dependency_keys.len())
            .and_then(|count| count.checked_add(self.resolution.candidate_count()))
            .ok_or(LegacyWireError::SizeOverflow {
                resource: "reference nested collection",
            })
    }

    fn degradation_count_upper_bound(&self) -> Result<usize, LegacyWireError> {
        usize::from(self.source_object.is_some())
            .checked_add(self.diagnostics.len())
            .and_then(|count| count.checked_add(self.dependency_keys.len()))
            .and_then(|count| count.checked_add(self.resolution.address_count()))
            .ok_or(LegacyWireError::SizeOverflow {
                resource: "reference degradation capacity",
            })
    }

    pub(crate) fn try_into_current(
        self,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceProjectionFact, LegacyWireError> {
        let degradation_capacity = self.degradation_count_upper_bound()?;
        let Self {
            source_object,
            source_file_id,
            source_class_id,
            field_path,
            raw_target,
            resolution,
            diagnostics: legacy_diagnostics,
            dependency_keys: legacy_dependency_keys,
        } = self;
        let mut degradations = Vec::new();
        degradations
            .try_reserve_exact(degradation_capacity)
            .map_err(|_| LegacyWireError::AllocationFailed {
                resource: "reference degradations",
                requested: degradation_capacity,
            })?;
        let source_object = match source_object {
            Some(address) => {
                convert_address_or_degrade(address, "source object", &mut degradations)?
            }
            None => None,
        };
        let resolution = resolution.try_into_current_with_degradation(&mut degradations)?;

        let mut diagnostics = Vec::new();
        diagnostics
            .try_reserve_exact(legacy_diagnostics.len())
            .map_err(|_| LegacyWireError::AllocationFailed {
                resource: "reference diagnostics",
                requested: legacy_diagnostics.len(),
            })?;
        for diagnostic in legacy_diagnostics {
            diagnostics.push(diagnostic.try_into_current_with_degradation(&mut degradations)?);
        }

        let mut dependency_keys = Vec::new();
        dependency_keys
            .try_reserve_exact(legacy_dependency_keys.len())
            .map_err(|_| LegacyWireError::AllocationFailed {
                resource: "reference dependency keys",
                requested: legacy_dependency_keys.len(),
            })?;
        for dependency in legacy_dependency_keys {
            if let Some(dependency) =
                dependency.try_into_current_with_degradation(&mut degradations)?
            {
                dependency_keys.push(dependency);
            }
        }
        dependency_keys.sort_unstable();
        dependency_keys.dedup();

        degradations.sort_unstable();
        degradations.dedup();
        charge_degradation_field_path_clones(&field_path, degradations.len(), budget)?;
        diagnostics
            .try_reserve_exact(degradations.len())
            .map_err(|_| LegacyWireError::AllocationFailed {
                resource: "reference degradation diagnostics",
                requested: degradations.len(),
            })?;
        for degradation in degradations {
            diagnostics.push(degradation.into_diagnostic(field_path.clone())?);
        }
        diagnostics.sort_unstable();
        diagnostics.dedup();

        Ok(ReferenceProjectionFact {
            source_object,
            source_file_id,
            source_class_id,
            field_path,
            raw_target,
            resolution,
            diagnostics,
            dependency_keys,
        })
    }
}

/// Frozen analysis payload embedded in storage-v1 source state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyAssetAnalysisV1 {
    source: AnalyzedSource,
    search: SearchFacts,
    #[serde(
        default,
        skip_serializing_if = "LegacyWorkspaceGraphInputsV1::is_empty"
    )]
    graph_inputs: LegacyWorkspaceGraphInputsV1,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    references: Vec<LegacyReferenceProjectionFactV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    container_entries: Vec<ContainerEntryFact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    diagnostics: Vec<LegacyDiagnosticV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    truncations: Vec<AnalysisTruncation>,
    complete: bool,
}

impl LegacyAssetAnalysisV1 {
    #[must_use]
    pub(crate) fn relative_path(&self) -> &str {
        &self.source.relative_path
    }

    #[must_use]
    pub(crate) const fn complete(&self) -> bool {
        self.complete
    }

    pub(crate) fn nested_collection_count(&self) -> Result<usize, LegacyWireError> {
        let mut count = self
            .search
            .hierarchy_paths
            .len()
            .checked_add(self.search.script_symbols.len())
            .and_then(|value| value.checked_add(self.search.referenced_script_guids.len()))
            .and_then(|value| value.checked_add(self.graph_inputs.objects.len()))
            .and_then(|value| value.checked_add(self.references.len()))
            .and_then(|value| value.checked_add(self.container_entries.len()))
            .and_then(|value| value.checked_add(self.diagnostics.len()))
            .and_then(|value| value.checked_add(self.truncations.len()))
            .ok_or(LegacyWireError::SizeOverflow {
                resource: "analysis nested collection",
            })?;
        for reference in &self.references {
            count = count
                .checked_add(reference.nested_collection_count()?)
                .ok_or(LegacyWireError::SizeOverflow {
                    resource: "analysis nested collection",
                })?;
        }
        Ok(count)
    }

    pub(crate) fn validate_canonical_order(&self) -> Result<(), LegacyWireError> {
        let canonical = is_strictly_sorted(&self.search.hierarchy_paths)
            && is_strictly_sorted(&self.search.script_symbols)
            && is_strictly_sorted(&self.search.referenced_script_guids)
            && self.graph_inputs.validate_canonical_order()
            && is_strictly_sorted(&self.references)
            && is_strictly_sorted(&self.container_entries)
            && is_strictly_sorted(&self.diagnostics)
            && is_strictly_sorted(&self.truncations)
            && (self.truncations.is_empty() || !self.complete)
            && self
                .references
                .iter()
                .all(LegacyReferenceProjectionFactV1::validate_canonical_order);
        if canonical {
            Ok(())
        } else {
            Err(LegacyWireError::NonCanonicalAnalysis {
                relative_path: self.source.relative_path.clone(),
            })
        }
    }
}

/// Strict storage-v1 source-state payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacySourceStateSnapshotV1 {
    contract_version: u16,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    transaction_receipts: TransactionReceiptWindow,
    scan_hints: Vec<SourceScanHint>,
    assets: Vec<LegacyAssetAnalysisV1>,
    logical_digest: DigestV1,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySourceStateSnapshotWireV1 {
    contract_version: u16,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    transaction_receipts: TransactionReceiptWindow,
    scan_hints: Vec<SourceScanHint>,
    assets: Vec<LegacyAssetAnalysisV1>,
    logical_digest: DigestV1,
}

#[derive(Serialize)]
struct LegacySourceStateLogicalRefV1<'state> {
    contract_version: u16,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    transaction_receipts: &'state TransactionReceiptWindow,
    assets: &'state [LegacyAssetAnalysisV1],
}

impl LegacySourceStateSnapshotV1 {
    #[must_use]
    pub(crate) const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub(crate) const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub(crate) const fn transaction_receipts(&self) -> &TransactionReceiptWindow {
        &self.transaction_receipts
    }

    #[must_use]
    pub(crate) fn into_transaction_receipts(self) -> TransactionReceiptWindow {
        self.transaction_receipts
    }

    #[must_use]
    pub(crate) fn scan_hints(&self) -> &[SourceScanHint] {
        &self.scan_hints
    }

    #[must_use]
    pub(crate) fn assets(&self) -> &[LegacyAssetAnalysisV1] {
        &self.assets
    }

    #[must_use]
    pub(crate) const fn logical_digest(&self) -> DigestV1 {
        self.logical_digest
    }

    pub(crate) fn nested_collection_count(&self) -> Result<u64, LegacyWireError> {
        let mut entries = self
            .transaction_receipts
            .ids()
            .len()
            .checked_add(self.scan_hints.len())
            .and_then(|value| value.checked_add(self.assets.len()))
            .ok_or(LegacyWireError::SizeOverflow {
                resource: "source-state entries",
            })?;
        for analysis in &self.assets {
            entries = entries
                .checked_add(analysis.nested_collection_count()?)
                .ok_or(LegacyWireError::SizeOverflow {
                    resource: "source-state entries",
                })?;
        }
        u64::try_from(entries).map_err(|_| LegacyWireError::SizeOverflow {
            resource: "source-state entries",
        })
    }

    pub(crate) fn validate_canonical_order(&self) -> Result<(), LegacyWireError> {
        ensure_strictly_sorted_paths(
            "source-state scan hints",
            self.scan_hints
                .iter()
                .map(|hint| hint.relative_path.as_str()),
        )?;
        ensure_strictly_sorted_paths(
            "source-state assets",
            self.assets.iter().map(LegacyAssetAnalysisV1::relative_path),
        )?;
        for analysis in &self.assets {
            analysis.validate_canonical_order()?;
        }
        Ok(())
    }

    fn computed_logical_digest(&self) -> Result<DigestV1, LegacyWireError> {
        canonical_digest(&LegacySourceStateLogicalRefV1 {
            contract_version: LEGACY_WIRE_VERSION,
            workspace: self.workspace,
            revision: self.revision,
            transaction_receipts: &self.transaction_receipts,
            assets: &self.assets,
        })
    }
}

impl<'de> Deserialize<'de> for LegacySourceStateSnapshotV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = LegacySourceStateSnapshotWireV1::deserialize(deserializer)?;
        validate_version("source-state", wire.contract_version)
            .map_err(serde::de::Error::custom)?;
        validate_collection_size(
            "source-state scan hints",
            wire.scan_hints.len(),
            MAX_SOURCE_STATE_SCAN_HINTS,
        )
        .map_err(serde::de::Error::custom)?;
        validate_collection_size(
            "source-state assets",
            wire.assets.len(),
            MAX_SOURCE_STATE_ASSETS,
        )
        .map_err(serde::de::Error::custom)?;
        wire.transaction_receipts
            .validate_for_workspace(wire.workspace)
            .map_err(|error| {
                serde::de::Error::custom(LegacyWireError::InvalidTransactionReceipts {
                    message: error.to_string(),
                })
            })?;
        for hint in &wire.scan_hints {
            validate_relative_path(&hint.relative_path).map_err(serde::de::Error::custom)?;
        }
        for analysis in &wire.assets {
            validate_relative_path(analysis.relative_path()).map_err(serde::de::Error::custom)?;
        }

        let snapshot = Self {
            contract_version: LEGACY_WIRE_VERSION,
            workspace: wire.workspace,
            revision: wire.revision,
            transaction_receipts: wire.transaction_receipts,
            scan_hints: wire.scan_hints,
            assets: wire.assets,
            logical_digest: wire.logical_digest,
        };
        snapshot
            .validate_canonical_order()
            .map_err(serde::de::Error::custom)?;
        let actual = snapshot
            .computed_logical_digest()
            .map_err(serde::de::Error::custom)?;
        if actual != snapshot.logical_digest {
            return Err(serde::de::Error::custom(
                LegacyWireError::LogicalDigestMismatch {
                    expected: snapshot.logical_digest,
                    actual,
                },
            ));
        }
        Ok(snapshot)
    }
}

/// Strict storage-v1 reference-payload document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyReferencePayloadV1 {
    contract_version: u16,
    stable_id: String,
    source_path: String,
    source_kind: String,
    source_guid: Option<String>,
    source_object: Option<LegacyObjectAddressV1>,
    source_file_id: Option<i64>,
    source_class_id: Option<i32>,
    fact: LegacyReferenceProjectionFactV1,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyReferencePayloadWireV1 {
    contract_version: u16,
    stable_id: String,
    source_path: String,
    source_kind: String,
    #[serde(default)]
    source_guid: Option<String>,
    #[serde(default)]
    source_object: Option<LegacyObjectAddressV1>,
    #[serde(default)]
    source_file_id: Option<i64>,
    #[serde(default)]
    source_class_id: Option<i32>,
    fact: LegacyReferenceProjectionFactV1,
}

impl LegacyReferencePayloadV1 {
    pub(crate) fn validate(&self, expected_stable_id: &str) -> Result<(), LegacyWireError> {
        if self.stable_id.is_empty() {
            return Err(LegacyWireError::EmptyStableId);
        }
        if self.stable_id != expected_stable_id {
            return Err(LegacyWireError::StableIdMismatch);
        }
        if self.source_object != self.fact.source_object
            || self.source_file_id != self.fact.source_file_id
            || self.source_class_id != self.fact.source_class_id
        {
            return Err(LegacyWireError::SourceIdentityMismatch);
        }
        Ok(())
    }

    pub(crate) fn try_into_current(
        self,
        expected_stable_id: &str,
        budget: &mut AssetLoadBudget,
    ) -> Result<ConvertedLegacyReferencePayloadV1, LegacyWireError> {
        self.validate(expected_stable_id)?;
        let fact = self.fact.try_into_current(budget)?;
        let source_object = fact.source_object.clone();
        Ok(ConvertedLegacyReferencePayloadV1 {
            stable_id: self.stable_id,
            source_path: self.source_path,
            source_kind: self.source_kind,
            source_guid: self.source_guid,
            source_object,
            source_file_id: self.source_file_id,
            source_class_id: self.source_class_id,
            fact,
        })
    }
}

impl<'de> Deserialize<'de> for LegacyReferencePayloadV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = LegacyReferencePayloadWireV1::deserialize(deserializer)?;
        validate_version("reference payload", wire.contract_version)
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            contract_version: LEGACY_WIRE_VERSION,
            stable_id: wire.stable_id,
            source_path: wire.source_path,
            source_kind: wire.source_kind,
            source_guid: wire.source_guid,
            source_object: wire.source_object,
            source_file_id: wire.source_file_id,
            source_class_id: wire.source_class_id,
            fact: wire.fact,
        })
    }
}

/// Current-domain payload fields consumed by the reference query adapter.
pub(crate) struct ConvertedLegacyReferencePayloadV1 {
    pub(crate) stable_id: String,
    pub(crate) source_path: String,
    pub(crate) source_kind: String,
    pub(crate) source_guid: Option<String>,
    pub(crate) source_object: Option<ObjectAddress>,
    pub(crate) source_file_id: Option<i64>,
    pub(crate) source_class_id: Option<i32>,
    pub(crate) fact: ReferenceProjectionFact,
}

fn convert_address_or_degrade(
    address: LegacyObjectAddressV1,
    context: &'static str,
    degradations: &mut Vec<LegacyAddressDegradation>,
) -> Result<Option<ObjectAddress>, LegacyWireError> {
    match address.try_into_current() {
        Ok(address) => Ok(Some(address)),
        Err(LegacyWireError::UnrepresentableYamlAnchor(anchor)) => {
            degradations.push(LegacyAddressDegradation { context, anchor });
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct LegacyAddressDegradation {
    context: &'static str,
    anchor: String,
}

impl LegacyAddressDegradation {
    fn into_diagnostic(self, field_path: FieldPath) -> Result<Diagnostic, LegacyWireError> {
        let message = format!(
            "Legacy {} uses YAML anchor {:?}, which is not a canonical nonzero i64 fileID",
            self.context, self.anchor
        );
        Diagnostic::new(
            DiagnosticSeverity::Warning,
            "LEGACY_YAML_ADDRESS_UNREPRESENTABLE",
            message,
        )
        .map_err(LegacyWireError::DiagnosticConversion)
        .map(|diagnostic| diagnostic.at_field(field_path))
    }
}

fn charge_degradation_field_path_clones(
    field_path: &FieldPath,
    clone_count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), LegacyWireError> {
    if clone_count == 0 {
        return Ok(());
    }
    let clone_count = u64::try_from(clone_count).map_err(|_| LegacyWireError::SizeOverflow {
        resource: "degradation field-path clone count",
    })?;
    let segments_per_clone =
        u64::try_from(field_path.segments().len()).map_err(|_| LegacyWireError::SizeOverflow {
            resource: "degradation field-path segments",
        })?;
    let segments =
        segments_per_clone
            .checked_mul(clone_count)
            .ok_or(LegacyWireError::SizeOverflow {
                resource: "degradation field-path segments",
            })?;
    let retained_bytes_per_clone = field_path
        .retained_clone_bytes()
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(LegacyWireError::SizeOverflow {
            resource: "degradation field-path allocation",
        })?;
    let retained_bytes =
        retained_bytes_per_clone
            .checked_mul(clone_count)
            .ok_or(LegacyWireError::SizeOverflow {
                resource: "degradation field-path allocation",
            })?;

    budget
        .check_entries(segments)
        .map_err(LegacyWireError::Budget)?;
    budget
        .check_members(segments)
        .map_err(LegacyWireError::Budget)?;
    budget
        .check_bytes(retained_bytes)
        .map_err(LegacyWireError::Budget)?;
    budget
        .consume_entries(segments)
        .map_err(LegacyWireError::Budget)?;
    budget
        .consume_members(segments)
        .map_err(LegacyWireError::Budget)?;
    budget
        .consume_bytes(retained_bytes)
        .map_err(LegacyWireError::Budget)?;

    Ok(())
}

fn validate_version(contract: &'static str, actual: u16) -> Result<(), LegacyWireError> {
    if actual == LEGACY_WIRE_VERSION {
        Ok(())
    } else {
        Err(LegacyWireError::UnsupportedVersion {
            contract,
            actual,
            expected: LEGACY_WIRE_VERSION,
        })
    }
}

fn validate_legacy_yaml_anchor(anchor: &str) -> Result<(), LegacyWireError> {
    if anchor.is_empty()
        || anchor.len() > MAX_YAML_ANCHOR_BYTES
        || !anchor
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        Err(LegacyWireError::InvalidYamlAnchor(anchor.to_owned()))
    } else {
        Ok(())
    }
}

fn validate_diagnostic_text(code: &str, message: &str) -> Result<(), LegacyWireError> {
    if code.len() > MAX_DIAGNOSTIC_CODE_BYTES {
        return Err(LegacyWireError::DiagnosticCodeTooLong);
    }
    if code.is_empty()
        || !code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(LegacyWireError::InvalidDiagnosticCode(code.to_owned()));
    }
    if message.len() > MAX_DIAGNOSTIC_MESSAGE_BYTES {
        return Err(LegacyWireError::DiagnosticMessageTooLong);
    }
    if message.is_empty() {
        return Err(LegacyWireError::EmptyDiagnosticMessage);
    }
    Ok(())
}

fn validate_collection_size(
    collection: &'static str,
    actual: usize,
    maximum: usize,
) -> Result<(), LegacyWireError> {
    if actual > maximum {
        Err(LegacyWireError::CollectionTooLarge {
            collection,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn validate_relative_path(relative_path: &str) -> Result<(), LegacyWireError> {
    if relative_path.is_empty()
        || relative_path.len() > MAX_PORTABLE_PATH_BYTES
        || relative_path.starts_with('/')
        || relative_path
            .chars()
            .any(|character| matches!(character, '\\' | '\0' | ':'))
        || relative_path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        Err(LegacyWireError::InvalidRelativePath {
            relative_path: relative_path.to_owned(),
            maximum_bytes: MAX_PORTABLE_PATH_BYTES,
        })
    } else {
        Ok(())
    }
}

fn is_strictly_sorted<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn ensure_strictly_sorted_paths<'path>(
    collection: &'static str,
    paths: impl IntoIterator<Item = &'path str>,
) -> Result<(), LegacyWireError> {
    let mut previous = None;
    for path in paths {
        if matches!(previous, Some(previous) if previous >= path) {
            return Err(LegacyWireError::NonCanonicalOrder { collection });
        }
        previous = Some(path);
    }
    Ok(())
}

fn canonical_digest(value: &impl Serialize) -> Result<DigestV1, LegacyWireError> {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value).map_err(LegacyWireError::Json)?;
    let mut builder = DigestV1Builder::new(counter.bytes);
    serde_json::to_writer(DigestWriter(&mut builder), value).map_err(LegacyWireError::Json)?;
    builder.finalize().map_err(LegacyWireError::Digest)
}

#[derive(Default)]
struct ByteCounter {
    bytes: u64,
}

impl Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len() as u64)
            .ok_or_else(|| io::Error::other("legacy canonical JSON length overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DigestWriter<'builder>(&'builder mut DigestV1Builder);

impl Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .update(buffer)
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::AssetLoadLimits;
    #[test]
    fn noncanonical_numeric_anchor_preserves_v1_bytes_but_cannot_convert() {
        let encoded = br#"{
            "kind":"yaml",
            "version":1,
            "source":{"version":1,"outer_path":"Assets/Test.prefab","members":[]},
            "selector":{"kind":"anchored","anchor":"01"}
        }"#;
        let legacy: LegacyObjectAddressV1 = serde_json::from_slice(encoded).unwrap();

        assert!(matches!(
            legacy.try_into_current(),
            Err(LegacyWireError::UnrepresentableYamlAnchor(anchor)) if anchor == "01"
        ));
    }

    #[test]
    fn canonical_numeric_anchor_converts_to_current_yaml_address() {
        let encoded = br#"{
            "kind":"yaml",
            "version":1,
            "source":{"version":1,"outer_path":"Assets/Test.prefab","members":[]},
            "selector":{"kind":"anchored","anchor":"-42"}
        }"#;
        let legacy: LegacyObjectAddressV1 = serde_json::from_slice(encoded).unwrap();
        let current = legacy.try_into_current().unwrap();

        assert_eq!(current.yaml_file_id().map(YamlFileId::get), Some(-42));
    }

    #[test]
    fn object_address_rejects_future_version_and_unknown_fields() {
        let future = br#"{
            "kind":"yaml",
            "version":2,
            "source":{"version":1,"outer_path":"Assets/Test.prefab","members":[]},
            "selector":{"kind":"unanchored","document_index":0}
        }"#;
        assert!(serde_json::from_slice::<LegacyObjectAddressV1>(future).is_err());

        let unknown = br#"{
            "kind":"yaml",
            "version":1,
            "source":{"version":1,"outer_path":"Assets/Test.prefab","members":[]},
            "selector":{"kind":"unanchored","document_index":0,"extra":true}
        }"#;
        assert!(serde_json::from_slice::<LegacyObjectAddressV1>(unknown).is_err());
    }

    #[test]
    fn unrepresentable_payload_addresses_degrade_locally() {
        let encoded = br#"{
            "contract_version":1,
            "stable_id":"reference-v1:test",
            "source_path":"Assets/Test.prefab",
            "source_kind":"Prefab",
            "source_object":{
                "kind":"yaml",
                "version":1,
                "source":{"version":1,"outer_path":"Assets/Test.prefab","members":[]},
                "selector":{"kind":"anchored","anchor":"01"}
            },
            "source_file_id":1,
            "source_class_id":114,
            "fact":{
                "source_object":{
                    "kind":"yaml",
                    "version":1,
                    "source":{"version":1,"outer_path":"Assets/Test.prefab","members":[]},
                    "selector":{"kind":"anchored","anchor":"01"}
                },
                "source_file_id":1,
                "source_class_id":114,
                "field_path":[],
                "raw_target":{"format":"yaml","file_id":1},
                "resolution":{
                    "state":"resolved",
                    "target":{
                        "kind":"yaml",
                        "version":1,
                        "source":{"version":1,"outer_path":"Assets/Test.prefab","members":[]},
                        "selector":{"kind":"anchored","anchor":"01"}
                    }
                },
                "diagnostics":[],
                "dependency_keys":[{
                    "kind":"object",
                    "address":{
                        "kind":"yaml",
                        "version":1,
                        "source":{"version":1,"outer_path":"Assets/Test.prefab","members":[]},
                        "selector":{"kind":"anchored","anchor":"01"}
                    }
                }]
            }
        }"#;
        let payload: LegacyReferencePayloadV1 = serde_json::from_slice(encoded).unwrap();

        let converted = payload
            .try_into_current("reference-v1:test", &mut AssetLoadBudget::default())
            .unwrap();

        assert!(converted.source_object.is_none());
        assert!(converted.fact.source_object.is_none());
        assert!(matches!(
            converted.fact.resolution,
            ReferenceResolutionProjection::Invalid
        ));
        assert!(converted.fact.dependency_keys.is_empty());
        assert_eq!(converted.fact.diagnostics.len(), 3);
        assert!(
            converted
                .fact
                .diagnostics
                .iter()
                .all(|diagnostic| { diagnostic.code() == "LEGACY_YAML_ADDRESS_UNREPRESENTABLE" })
        );
    }

    #[test]
    fn degradation_field_path_clone_is_rejected_one_byte_before_allocation() {
        let encoded = br#"{
            "contract_version":1,
            "stable_id":"reference-v1:test",
            "source_path":"Assets/Test.prefab",
            "source_kind":"Prefab",
            "source_object":{
                "kind":"yaml",
                "version":1,
                "source":{"version":1,"outer_path":"Assets/Test.prefab","members":[]},
                "selector":{"kind":"anchored","anchor":"01"}
            },
            "source_file_id":1,
            "source_class_id":114,
            "fact":{
                "source_object":{
                    "kind":"yaml",
                    "version":1,
                    "source":{"version":1,"outer_path":"Assets/Test.prefab","members":[]},
                    "selector":{"kind":"anchored","anchor":"01"}
                },
                "source_file_id":1,
                "source_class_id":114,
                "field_path":[],
                "raw_target":{"format":"yaml","file_id":1},
                "resolution":{"state":"null"},
                "diagnostics":[],
                "dependency_keys":[]
            }
        }"#;
        let mut payload: LegacyReferencePayloadV1 = serde_json::from_slice(encoded).unwrap();
        payload.fact.field_path = FieldPath::root().push_field("field".repeat(1024)).unwrap();
        let retained_bytes = u64::try_from(
            payload
                .fact
                .field_path
                .retained_clone_bytes()
                .expect("test field path size must be representable"),
        )
        .unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: retained_bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = match payload.try_into_current("reference-v1:test", &mut budget) {
            Ok(_) => panic!("one-short conversion budget must reject the field-path clone"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            LegacyWireError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == retained_bytes - 1 && requested == retained_bytes
        ));
        assert_eq!(budget.usage().bytes, 0);
        assert_eq!(budget.usage().entries, 0);
        assert_eq!(budget.usage().members, 0);
    }

    #[test]
    fn repeated_degradations_share_one_budgeted_field_path_clone() {
        let encoded = br#"{
            "contract_version":1,
            "stable_id":"reference-v1:test",
            "source_path":"Assets/Test.prefab",
            "source_kind":"Prefab",
            "source_file_id":1,
            "source_class_id":114,
            "fact":{
                "source_file_id":1,
                "source_class_id":114,
                "field_path":[],
                "raw_target":{"format":"yaml","file_id":1},
                "resolution":{
                    "state":"ambiguous",
                    "candidates":[{
                        "kind":"yaml",
                        "version":1,
                        "source":{"version":1,"outer_path":"Assets/Test.prefab","members":[]},
                        "selector":{"kind":"anchored","anchor":"01"}
                    }]
                },
                "diagnostics":[],
                "dependency_keys":[]
            }
        }"#;
        let mut payload: LegacyReferencePayloadV1 = serde_json::from_slice(encoded).unwrap();
        payload.fact.field_path = FieldPath::root().push_field("field".repeat(1024)).unwrap();
        let LegacyReferenceResolutionProjectionV1::Ambiguous { candidates } =
            &mut payload.fact.resolution
        else {
            panic!("test fixture must contain ambiguous candidates");
        };
        let candidate = candidates[0].clone();
        candidates.resize(1024, candidate);
        let retained_bytes = u64::try_from(
            payload
                .fact
                .field_path
                .retained_clone_bytes()
                .expect("test field path size must be representable"),
        )
        .unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_bytes: retained_bytes,
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let converted = payload
            .try_into_current("reference-v1:test", &mut budget)
            .unwrap();

        assert!(matches!(
            converted.fact.resolution,
            ReferenceResolutionProjection::Invalid
        ));
        assert_eq!(converted.fact.diagnostics.len(), 1);
        assert_eq!(budget.usage().bytes, retained_bytes);
        assert_eq!(budget.usage().entries, 1);
        assert_eq!(budget.usage().members, 1);
    }
}
