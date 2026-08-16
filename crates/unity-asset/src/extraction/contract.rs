//! Shared value types for versioned extraction contracts.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use unity_asset_core::{AssetLoadBudget, ObjectAddress, SourceFingerprint, SourceLocator};
use unity_asset_write::artifact::{ArtifactNameError, LogicalArtifactName};

/// A validated relative output path that has portable filesystem semantics.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtractionPath {
    name: LogicalArtifactName,
}

impl ExtractionPath {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ArtifactNameError> {
        Ok(Self {
            name: LogicalArtifactName::new(value)?,
        })
    }

    pub(crate) fn from_string_with_budget(
        value: String,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ArtifactNameError> {
        Ok(Self {
            name: LogicalArtifactName::from_string_with_budget(value, budget)?,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.name.as_str()
    }

    pub(crate) fn portability_key(&self) -> &str {
        self.name.portability_key()
    }

    #[cfg(test)]
    pub(crate) fn retained_bytes(&self) -> Result<u64, ArtifactNameError> {
        self.name.retained_bytes()
    }
}

impl PartialOrd for ExtractionPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ExtractionPath {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
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

/// Unit attached to a fallible extraction allocation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractionAllocationUnit {
    Bytes,
    CapacityUnits,
}

impl ExtractionAllocationUnit {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bytes => "bytes",
            Self::CapacityUnits => "capacity_units",
        }
    }
}

impl fmt::Display for ExtractionAllocationUnit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
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

    pub(in crate::extraction) fn into_parts(self) -> (SourceLocator, SourceFingerprint) {
        (self.locator, self.fingerprint)
    }
}

/// Stable, machine-actionable extraction diagnostic categories.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionDiagnosticCode {
    DecodedUnavailable,
    FeatureUnavailable,
    UnsupportedClass,
    UnsupportedMediaEncoding,
    UnsupportedMediaLayout,
    DecodeFailedRawFallback,
    MissingResource,
    UnresolvedDependency,
    UnresolvedSpritePPtr,
    SourceChanged,
    OutputExists,
    OutputFailed,
    OutputLimitExceeded,
    ResumeMismatch,
    StoppedAfterFailure,
}

/// A deterministic diagnostic that never persists operating-system error text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionDiagnostic {
    code: ExtractionDiagnosticCode,
    address: Option<ObjectAddress>,
}

impl ExtractionDiagnostic {
    #[must_use]
    pub const fn new(code: ExtractionDiagnosticCode, address: Option<ObjectAddress>) -> Self {
        Self { code, address }
    }

    #[must_use]
    pub const fn code(&self) -> ExtractionDiagnosticCode {
        self.code
    }

    #[must_use]
    pub const fn address(&self) -> Option<&ObjectAddress> {
        self.address.as_ref()
    }
}
