//! Canonical extraction manifests and deterministic execution reports.

use std::io::{self, Read, Write};

use serde::ser::{SerializeSeq, SerializeStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetedJsonError, DigestBuildError, DigestV1, DigestV1Builder, ObjectAddress,
    WorkspaceId, WorkspaceRevision,
};

use super::CheckedByteCounter;
use super::contract::{
    ExtractionArtifactKind, ExtractionDiagnostic, ExtractionDiagnosticCode, ExtractionPath,
    ExtractionSourceExpectation,
};
use super::json_contract::{large_contract_limits, read_json_bounded};
use super::model::{
    ExtractionModelError, ExtractionPlan, ExtractionRequest, PlannedArtifact, first_path_conflict,
    normalize_source_expectations, normalize_values,
};

pub const EXTRACTION_MANIFEST_VERSION: u8 = 4;
pub const EXTRACTION_REPORT_VERSION: u8 = 4;
pub const EXTRACTION_MANIFEST_CONTRACT: &str = "unity_asset.extraction_manifest";
pub const EXTRACTION_REPORT_CONTRACT: &str = "unity_asset.extraction_report";

const EXTRACTION_MANIFEST_JSON_LIMITS: unity_asset_core::ContractJsonLimits =
    large_contract_limits(EXTRACTION_MANIFEST_CONTRACT);
const EXTRACTION_REPORT_JSON_LIMITS: unity_asset_core::ContractJsonLimits =
    large_contract_limits(EXTRACTION_REPORT_CONTRACT);

/// Stable outcome of one planned output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionArtifactStatus {
    Written,
    Resumed,
    SkippedExisting,
    Failed,
}

/// One ordered artifact receipt in an extraction manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionManifestArtifact {
    ordinal: u32,
    address: ObjectAddress,
    kind: ExtractionArtifactKind,
    path: ExtractionPath,
    status: ExtractionArtifactStatus,
    length: Option<u64>,
    digest: Option<DigestV1>,
    diagnostics: Box<[ExtractionDiagnostic]>,
}

impl<'de> Deserialize<'de> for ExtractionManifestArtifact {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ExtractionManifestArtifactWire::deserialize(deserializer)?
            .into_artifact()
            .map_err(serde::de::Error::custom)
    }
}

/// Public name for one persisted artifact receipt.
pub type ExtractionArtifactRecord = ExtractionManifestArtifact;

impl ExtractionManifestArtifact {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ordinal: u32,
        address: ObjectAddress,
        kind: ExtractionArtifactKind,
        path: ExtractionPath,
        status: ExtractionArtifactStatus,
        length: Option<u64>,
        digest: Option<DigestV1>,
        diagnostics: Vec<ExtractionDiagnostic>,
    ) -> Result<Self, ExtractionManifestError> {
        validate_artifact_evidence(status, length, digest)?;
        if diagnostics
            .iter()
            .any(|diagnostic| diagnostic.address() != Some(&address))
        {
            return Err(ExtractionManifestError::InvalidDiagnosticAddress { ordinal });
        }
        Ok(Self {
            ordinal,
            address,
            kind,
            path,
            status,
            length,
            digest,
            diagnostics: normalize_diagnostics(diagnostics).into_boxed_slice(),
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
    pub const fn kind(&self) -> ExtractionArtifactKind {
        self.kind
    }

    #[must_use]
    pub const fn path(&self) -> &ExtractionPath {
        &self.path
    }

    #[must_use]
    pub const fn status(&self) -> ExtractionArtifactStatus {
        self.status
    }

    #[must_use]
    pub const fn length(&self) -> Option<u64> {
        self.length
    }

    #[must_use]
    pub const fn digest(&self) -> Option<DigestV1> {
        self.digest
    }

    #[must_use]
    pub const fn diagnostics(&self) -> &[ExtractionDiagnostic] {
        &self.diagnostics
    }

    fn merge_diagnostics(&mut self, additional: &[ExtractionDiagnostic]) {
        if additional.is_empty() {
            return;
        }
        let mut diagnostics = self.diagnostics.to_vec();
        diagnostics.extend_from_slice(additional);
        self.diagnostics = normalize_diagnostics(diagnostics).into_boxed_slice();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractionManifestArtifactWire {
    ordinal: u32,
    address: ObjectAddress,
    kind: ExtractionArtifactKind,
    path: ExtractionPath,
    status: ExtractionArtifactStatus,
    length: Option<u64>,
    digest: Option<DigestV1>,
    diagnostics: Vec<ExtractionDiagnostic>,
}

impl ExtractionManifestArtifactWire {
    fn into_artifact(self) -> Result<ExtractionManifestArtifact, ExtractionManifestError> {
        ExtractionManifestArtifact::new(
            self.ordinal,
            self.address,
            self.kind,
            self.path,
            self.status,
            self.length,
            self.digest,
            self.diagnostics,
        )
    }
}

/// Canonical, resumable evidence for one extraction plan execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionManifest {
    workspace_id: WorkspaceId,
    revision: WorkspaceRevision,
    request: ExtractionRequest,
    request_digest: DigestV1,
    plan_digest: DigestV1,
    sources: Box<[ExtractionSourceExpectation]>,
    artifacts: Box<[ExtractionManifestArtifact]>,
}

impl ExtractionManifest {
    pub(crate) fn new(
        plan: &ExtractionPlan,
        mut artifacts: Vec<ExtractionManifestArtifact>,
    ) -> Result<Self, ExtractionManifestError> {
        if artifacts.len() != plan.artifacts().len() {
            return Err(ExtractionManifestError::ArtifactCountMismatch {
                expected: plan.artifacts().len(),
                actual: artifacts.len(),
            });
        }
        for (planned, artifact) in plan.artifacts().iter().zip(&mut artifacts) {
            if planned.ordinal() != artifact.ordinal
                || planned.address() != &artifact.address
                || !planned.matches_output(artifact.kind, &artifact.path)
            {
                return Err(ExtractionManifestError::ArtifactDoesNotMatchPlan {
                    ordinal: artifact.ordinal,
                });
            }
            artifact.merge_diagnostics(planned.diagnostics());
        }
        Self::from_parts(
            plan.workspace_id(),
            plan.revision(),
            plan.request().clone(),
            plan.request_digest(),
            plan.digest()?,
            plan.sources().to_vec(),
            artifacts,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        workspace_id: WorkspaceId,
        revision: WorkspaceRevision,
        request: ExtractionRequest,
        request_digest: DigestV1,
        plan_digest: DigestV1,
        sources: Vec<ExtractionSourceExpectation>,
        artifacts: Vec<ExtractionManifestArtifact>,
    ) -> Result<Self, ExtractionManifestError> {
        let actual_request_digest = request.digest()?;
        if request_digest != actual_request_digest {
            return Err(ExtractionManifestError::RequestDigestMismatch {
                declared: request_digest,
                actual: actual_request_digest,
            });
        }
        let sources = normalize_source_expectations(sources)?;
        validate_manifest_artifacts(&artifacts)?;
        Ok(Self {
            workspace_id,
            revision,
            request,
            request_digest,
            plan_digest,
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
    pub const fn plan_digest(&self) -> DigestV1 {
        self.plan_digest
    }

    #[must_use]
    pub const fn sources(&self) -> &[ExtractionSourceExpectation] {
        &self.sources
    }

    #[must_use]
    pub const fn artifacts(&self) -> &[ExtractionManifestArtifact] {
        &self.artifacts
    }

    #[must_use]
    pub fn artifact_by_ordinal(&self, ordinal: u32) -> Option<&ExtractionManifestArtifact> {
        usize::try_from(ordinal)
            .ok()
            .and_then(|index| self.artifacts.get(index))
            .filter(|artifact| artifact.ordinal == ordinal)
    }

    #[must_use]
    pub fn artifact_by_address(
        &self,
        address: &ObjectAddress,
    ) -> Option<&ExtractionManifestArtifact> {
        self.artifacts
            .iter()
            .find(|artifact| &artifact.address == address)
    }

    #[must_use]
    pub fn artifact_by_path(&self, path: &str) -> Option<&ExtractionManifestArtifact> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.path.as_str() == path)
    }

    #[must_use]
    pub fn resume_candidate(
        &self,
        ordinal: u32,
        address: &ObjectAddress,
        path: &ExtractionPath,
    ) -> Option<&ExtractionManifestArtifact> {
        self.artifact_by_ordinal(ordinal)
            .filter(|artifact| artifact.address() == address && artifact.path() == path)
    }

    pub fn read_json(
        reader: impl Read,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, BudgetedJsonError> {
        read_json_bounded(reader, budget, EXTRACTION_MANIFEST_JSON_LIMITS)
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
struct ExtractionManifestRef<'value> {
    contract: &'static str,
    version: u8,
    workspace_id: WorkspaceId,
    revision: WorkspaceRevision,
    request: &'value ExtractionRequest,
    request_digest: DigestV1,
    plan_digest: DigestV1,
    sources: &'value [ExtractionSourceExpectation],
    artifacts: &'value [ExtractionManifestArtifact],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractionManifestWire {
    contract: String,
    version: u8,
    workspace_id: WorkspaceId,
    revision: WorkspaceRevision,
    request: ExtractionRequest,
    request_digest: DigestV1,
    plan_digest: DigestV1,
    sources: Vec<ExtractionSourceExpectation>,
    artifacts: Vec<ExtractionManifestArtifactWire>,
}

impl Serialize for ExtractionManifest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ExtractionManifestRef {
            contract: EXTRACTION_MANIFEST_CONTRACT,
            version: EXTRACTION_MANIFEST_VERSION,
            workspace_id: self.workspace_id,
            revision: self.revision,
            request: &self.request,
            request_digest: self.request_digest,
            plan_digest: self.plan_digest,
            sources: &self.sources,
            artifacts: &self.artifacts,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExtractionManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ExtractionManifestWire::deserialize(deserializer)?;
        if wire.contract != EXTRACTION_MANIFEST_CONTRACT {
            return Err(serde::de::Error::custom(
                ExtractionManifestError::UnexpectedContract {
                    expected: EXTRACTION_MANIFEST_CONTRACT,
                    actual: wire.contract,
                },
            ));
        }
        if wire.version != EXTRACTION_MANIFEST_VERSION {
            return Err(serde::de::Error::custom(
                ExtractionManifestError::UnsupportedManifestVersion(wire.version),
            ));
        }
        let artifacts = wire
            .artifacts
            .into_iter()
            .map(ExtractionManifestArtifactWire::into_artifact)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::de::Error::custom)?;
        Self::from_parts(
            wire.workspace_id,
            wire.revision,
            wire.request,
            wire.request_digest,
            wire.plan_digest,
            wire.sources,
            artifacts,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Deterministic counts derived from a validated manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionReportCounts {
    written: u64,
    resumed: u64,
    skipped_existing: u64,
    failed: u64,
}

impl ExtractionReportCounts {
    #[must_use]
    pub const fn written(self) -> u64 {
        self.written
    }

    #[must_use]
    pub const fn resumed(self) -> u64 {
        self.resumed
    }

    #[must_use]
    pub const fn skipped_existing(self) -> u64 {
        self.skipped_existing
    }

    #[must_use]
    pub const fn failed(self) -> u64 {
        self.failed
    }
}

/// Execution result whose canonical evidence is the embedded manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionReport {
    manifest: ExtractionManifest,
    counts: ExtractionReportCounts,
}

impl ExtractionReport {
    pub(crate) fn new(manifest: ExtractionManifest) -> Result<Self, ExtractionManifestError> {
        let counts = report_counts(&manifest)?;
        Ok(Self { manifest, counts })
    }

    #[must_use]
    pub const fn manifest(&self) -> &ExtractionManifest {
        &self.manifest
    }

    #[must_use]
    pub const fn counts(&self) -> ExtractionReportCounts {
        self.counts
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

    pub fn canonical_manifest_json(&self) -> Result<Vec<u8>, ExtractionCanonicalError> {
        self.manifest.canonical_json()
    }

    pub fn write_canonical_manifest_json(
        &self,
        writer: impl Write,
    ) -> Result<(), ExtractionCanonicalError> {
        self.manifest.write_canonical_json(writer)
    }

    pub fn manifest_digest(&self) -> Result<DigestV1, ExtractionCanonicalError> {
        self.manifest.digest()
    }

    pub fn read_json(
        reader: impl Read,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, BudgetedJsonError> {
        read_json_bounded(reader, budget, EXTRACTION_REPORT_JSON_LIMITS)
    }
}

#[derive(Serialize)]
struct ExtractionReportRef<'value> {
    contract: &'static str,
    version: u8,
    manifest: &'value ExtractionManifest,
    counts: ExtractionReportCounts,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractionReportWire {
    contract: String,
    version: u8,
    manifest: ExtractionManifest,
    counts: ExtractionReportCounts,
}

impl Serialize for ExtractionReport {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ExtractionReportRef {
            contract: EXTRACTION_REPORT_CONTRACT,
            version: EXTRACTION_REPORT_VERSION,
            manifest: &self.manifest,
            counts: self.counts,
        }
        .serialize(serializer)
    }
}

const MAXIMUM_DIAGNOSTIC_CODES: &[ExtractionDiagnosticCode] = &[
    ExtractionDiagnosticCode::DecodedUnavailable,
    ExtractionDiagnosticCode::FeatureUnavailable,
    ExtractionDiagnosticCode::UnsupportedClass,
    ExtractionDiagnosticCode::UnsupportedMediaEncoding,
    ExtractionDiagnosticCode::UnsupportedMediaLayout,
    ExtractionDiagnosticCode::DecodeFailedRawFallback,
    ExtractionDiagnosticCode::MissingResource,
    ExtractionDiagnosticCode::UnresolvedDependency,
    ExtractionDiagnosticCode::UnresolvedSpritePPtr,
    ExtractionDiagnosticCode::SourceChanged,
    ExtractionDiagnosticCode::OutputExists,
    ExtractionDiagnosticCode::OutputFailed,
    ExtractionDiagnosticCode::OutputLimitExceeded,
    ExtractionDiagnosticCode::ResumeMismatch,
    ExtractionDiagnosticCode::StoppedAfterFailure,
];

/// A lazy serialization upper bound for every report that can arise from one plan.
///
/// The bound intentionally emits maximal valid receipt fields and every diagnostic code without
/// allocating receipt, diagnostic, or manifest collections. It is used only to reject a report
/// limit before output publication begins.
pub(crate) struct MaximumExtractionReport<'plan> {
    plan: &'plan ExtractionPlan,
    plan_digest: DigestV1,
    digest: DigestV1,
}

pub(crate) fn maximum_extraction_report(
    plan: &ExtractionPlan,
) -> Result<MaximumExtractionReport<'_>, ExtractionCanonicalError> {
    Ok(MaximumExtractionReport {
        plan,
        plan_digest: plan.digest()?,
        digest: DigestV1::hash_bytes(&[]),
    })
}

impl Serialize for MaximumExtractionReport<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("ExtractionReport", 3)?;
        state.serialize_field("contract", EXTRACTION_REPORT_CONTRACT)?;
        state.serialize_field("version", &EXTRACTION_REPORT_VERSION)?;
        state.serialize_field("manifest", &self.manifest())?;
        state.serialize_field(
            "counts",
            &ExtractionReportCounts {
                written: u64::MAX,
                resumed: u64::MAX,
                skipped_existing: u64::MAX,
                failed: u64::MAX,
            },
        )?;
        state.end()
    }
}

impl<'plan> MaximumExtractionReport<'plan> {
    pub(crate) const fn manifest(&self) -> MaximumExtractionManifest<'plan> {
        MaximumExtractionManifest {
            plan: self.plan,
            plan_digest: self.plan_digest,
            digest: self.digest,
        }
    }
}

pub(crate) struct MaximumExtractionManifest<'plan> {
    plan: &'plan ExtractionPlan,
    plan_digest: DigestV1,
    digest: DigestV1,
}

impl Serialize for MaximumExtractionManifest<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("ExtractionManifest", 9)?;
        state.serialize_field("contract", EXTRACTION_MANIFEST_CONTRACT)?;
        state.serialize_field("version", &EXTRACTION_MANIFEST_VERSION)?;
        state.serialize_field("workspace_id", &self.plan.workspace_id())?;
        state.serialize_field("revision", &self.plan.revision())?;
        state.serialize_field("request", self.plan.request())?;
        state.serialize_field("request_digest", &self.plan.request_digest())?;
        state.serialize_field("plan_digest", &self.plan_digest)?;
        state.serialize_field("sources", self.plan.sources())?;
        state.serialize_field(
            "artifacts",
            &MaximumManifestArtifacts {
                artifacts: self.plan.artifacts(),
                digest: self.digest,
            },
        )?;
        state.end()
    }
}

struct MaximumManifestArtifacts<'plan> {
    artifacts: &'plan [PlannedArtifact],
    digest: DigestV1,
}

impl Serialize for MaximumManifestArtifacts<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.artifacts.len()))?;
        for artifact in self.artifacts {
            sequence.serialize_element(&MaximumManifestArtifact {
                artifact,
                digest: self.digest,
            })?;
        }
        sequence.end()
    }
}

struct MaximumManifestArtifact<'plan> {
    artifact: &'plan PlannedArtifact,
    digest: DigestV1,
}

impl Serialize for MaximumManifestArtifact<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let (kind, path) = longest_planned_output(self.artifact);
        let mut state = serializer.serialize_struct("ExtractionManifestArtifact", 8)?;
        state.serialize_field("ordinal", &self.artifact.ordinal())?;
        state.serialize_field("address", self.artifact.address())?;
        state.serialize_field("kind", &kind)?;
        state.serialize_field("path", path)?;
        state.serialize_field("status", &ExtractionArtifactStatus::SkippedExisting)?;
        state.serialize_field("length", &Some(u64::MAX))?;
        state.serialize_field("digest", &Some(self.digest))?;
        state.serialize_field(
            "diagnostics",
            &MaximumManifestDiagnostics {
                planned: self.artifact.diagnostics(),
                address: self.artifact.address(),
            },
        )?;
        state.end()
    }
}

fn longest_planned_output(artifact: &PlannedArtifact) -> (ExtractionArtifactKind, &ExtractionPath) {
    let preferred = (artifact.preferred_kind(), artifact.preferred_path());
    let preferred_length =
        artifact_kind_wire_length(preferred.0).saturating_add(preferred.1.as_str().len());
    match artifact.fallback_kind().zip(artifact.fallback_path()) {
        Some(fallback)
            if artifact_kind_wire_length(fallback.0).saturating_add(fallback.1.as_str().len())
                > preferred_length =>
        {
            fallback
        }
        _ => preferred,
    }
}

const fn artifact_kind_wire_length(kind: ExtractionArtifactKind) -> usize {
    match kind {
        ExtractionArtifactKind::BinaryRaw => "binary_raw".len(),
        ExtractionArtifactKind::Yaml => "yaml".len(),
        ExtractionArtifactKind::Text => "text".len(),
        ExtractionArtifactKind::Audio => "audio".len(),
        ExtractionArtifactKind::TexturePng => "texture_png".len(),
        ExtractionArtifactKind::SpritePng => "sprite_png".len(),
    }
}

struct MaximumManifestDiagnostics<'plan> {
    planned: &'plan [ExtractionDiagnostic],
    address: &'plan ObjectAddress,
}

impl Serialize for MaximumManifestDiagnostics<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let maximum = self
            .planned
            .len()
            .saturating_add(MAXIMUM_DIAGNOSTIC_CODES.len());
        let mut sequence = serializer.serialize_seq(Some(maximum))?;
        for diagnostic in self.planned {
            sequence.serialize_element(diagnostic)?;
        }
        for code in MAXIMUM_DIAGNOSTIC_CODES {
            sequence.serialize_element(&MaximumManifestDiagnostic {
                code: *code,
                address: Some(self.address),
            })?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct MaximumManifestDiagnostic<'value> {
    code: ExtractionDiagnosticCode,
    address: Option<&'value ObjectAddress>,
}

impl<'de> Deserialize<'de> for ExtractionReport {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ExtractionReportWire::deserialize(deserializer)?;
        if wire.contract != EXTRACTION_REPORT_CONTRACT {
            return Err(serde::de::Error::custom(
                ExtractionManifestError::UnexpectedContract {
                    expected: EXTRACTION_REPORT_CONTRACT,
                    actual: wire.contract,
                },
            ));
        }
        if wire.version != EXTRACTION_REPORT_VERSION {
            return Err(serde::de::Error::custom(
                ExtractionManifestError::UnsupportedReportVersion(wire.version),
            ));
        }
        let report = Self::new(wire.manifest).map_err(serde::de::Error::custom)?;
        if report.counts != wire.counts {
            return Err(serde::de::Error::custom(
                ExtractionManifestError::ReportCountMismatch,
            ));
        }
        Ok(report)
    }
}

fn validate_manifest_artifacts(
    artifacts: &[ExtractionManifestArtifact],
) -> Result<(), ExtractionManifestError> {
    let mut addresses = Vec::with_capacity(artifacts.len());
    let mut paths = Vec::with_capacity(artifacts.len());
    for (index, artifact) in artifacts.iter().enumerate() {
        let expected = u32::try_from(index)
            .map_err(|_| ExtractionManifestError::ArtifactCountOverflow { count: index + 1 })?;
        if artifact.ordinal != expected {
            return Err(ExtractionManifestError::NonCanonicalArtifactOrdinal {
                index,
                expected,
                actual: artifact.ordinal,
            });
        }
        validate_artifact_evidence(artifact.status, artifact.length, artifact.digest)?;
        if artifact.status == ExtractionArtifactStatus::Failed && artifact.diagnostics.is_empty() {
            return Err(ExtractionManifestError::FailedArtifactWithoutDiagnostic {
                ordinal: artifact.ordinal,
            });
        }
        addresses.push(&artifact.address);
        paths.push(&artifact.path);
    }

    addresses.sort_unstable();
    if let Some(pair) = addresses.windows(2).find(|pair| pair[0] == pair[1]) {
        return Err(ExtractionManifestError::DuplicateArtifactAddress(
            (*pair[0]).clone(),
        ));
    }
    if let Some((first, second)) = first_path_conflict(&mut paths) {
        return Err(ExtractionManifestError::DuplicateArtifactPath {
            first: first.as_str().to_owned(),
            second: second.as_str().to_owned(),
        });
    }
    Ok(())
}

fn validate_artifact_evidence(
    status: ExtractionArtifactStatus,
    length: Option<u64>,
    digest: Option<DigestV1>,
) -> Result<(), ExtractionManifestError> {
    let complete = length.is_some() && digest.is_some();
    let absent = length.is_none() && digest.is_none();
    let valid = match status {
        ExtractionArtifactStatus::Written | ExtractionArtifactStatus::Resumed => complete,
        ExtractionArtifactStatus::SkippedExisting => complete || absent,
        ExtractionArtifactStatus::Failed => absent,
    };
    if !valid {
        return Err(ExtractionManifestError::InvalidArtifactEvidence { status });
    }
    Ok(())
}

fn normalize_diagnostics(diagnostics: Vec<ExtractionDiagnostic>) -> Vec<ExtractionDiagnostic> {
    normalize_values(diagnostics)
}

fn report_counts(
    manifest: &ExtractionManifest,
) -> Result<ExtractionReportCounts, ExtractionManifestError> {
    let count = |status| {
        u64::try_from(
            manifest
                .artifacts
                .iter()
                .filter(|artifact| artifact.status == status)
                .count(),
        )
        .map_err(|_| ExtractionManifestError::ArtifactCountOverflow {
            count: manifest.artifacts.len(),
        })
    };
    Ok(ExtractionReportCounts {
        written: count(ExtractionArtifactStatus::Written)?,
        resumed: count(ExtractionArtifactStatus::Resumed)?,
        skipped_existing: count(ExtractionArtifactStatus::SkippedExisting)?,
        failed: count(ExtractionArtifactStatus::Failed)?,
    })
}

pub(crate) fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, ExtractionCanonicalError> {
    serde_json::to_vec(value).map_err(ExtractionCanonicalError::Json)
}

pub(crate) fn write_canonical_json<T: Serialize>(
    mut writer: impl Write,
    value: &T,
) -> Result<(), ExtractionCanonicalError> {
    serde_json::to_writer(&mut writer, value).map_err(ExtractionCanonicalError::Json)
}

pub(crate) fn canonical_digest<T: Serialize>(
    value: &T,
) -> Result<DigestV1, ExtractionCanonicalError> {
    let mut counter = CheckedByteCounter::new("canonical extraction JSON length overflow");
    write_canonical_json(&mut counter, value)?;
    let mut builder = DigestV1Builder::new(counter.bytes());
    write_canonical_json(DigestWriter(&mut builder), value)?;
    builder.finalize().map_err(ExtractionCanonicalError::Digest)
}

struct DigestWriter<'builder>(&'builder mut DigestV1Builder);

impl Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.update(buffer).map_err(io::Error::other)?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExtractionArtifactStatus, ExtractionDiagnostic, ExtractionDiagnosticCode,
        ExtractionManifestArtifact, ExtractionManifestError,
    };
    use crate::extraction::{ExtractionArtifactKind, ExtractionPath};
    use unity_asset_core::{ObjectAddress, SourceLocator};

    #[test]
    fn artifact_rejects_diagnostic_for_another_object() {
        let source = SourceLocator::path("objects.assets").expect("valid source locator");
        let address = ObjectAddress::binary_at(source.clone(), 1).expect("valid address");
        let other = ObjectAddress::binary_at(source, 2).expect("valid address");
        let error = ExtractionManifestArtifact::new(
            0,
            address,
            ExtractionArtifactKind::BinaryRaw,
            ExtractionPath::new("objects/item.bin").expect("valid output path"),
            ExtractionArtifactStatus::Failed,
            None,
            None,
            vec![ExtractionDiagnostic::new(
                ExtractionDiagnosticCode::OutputFailed,
                Some(other),
            )],
        )
        .expect_err("diagnostic addresses must belong to their artifact");

        assert!(matches!(
            error,
            ExtractionManifestError::InvalidDiagnosticAddress { ordinal: 0 }
        ));
    }
}

/// Failure while producing canonical extraction contract bytes.
#[derive(Debug, Error)]
pub enum ExtractionCanonicalError {
    #[error("failed to encode canonical extraction JSON: {0}")]
    Json(serde_json::Error),
    #[error("failed to digest canonical extraction JSON: {0}")]
    Digest(DigestBuildError),
}

/// Validation failure for an extraction manifest or report.
#[derive(Debug, Error)]
pub enum ExtractionManifestError {
    #[error("extraction contract {actual:?} is unsupported; expected {expected:?}")]
    UnexpectedContract {
        expected: &'static str,
        actual: String,
    },
    #[error("extraction manifest version {0} is unsupported")]
    UnsupportedManifestVersion(u8),
    #[error("extraction report version {0} is unsupported")]
    UnsupportedReportVersion(u8),
    #[error("manifest request digest is {actual}, not declared digest {declared}")]
    RequestDigestMismatch {
        declared: DigestV1,
        actual: DigestV1,
    },
    #[error("manifest has {actual} artifacts; plan requires {expected}")]
    ArtifactCountMismatch { expected: usize, actual: usize },
    #[error("manifest artifact {ordinal} does not match its planned identity or output")]
    ArtifactDoesNotMatchPlan { ordinal: u32 },
    #[error("extraction manifest contains too many artifacts: {count}")]
    ArtifactCountOverflow { count: usize },
    #[error(
        "manifest artifact at index {index} has ordinal {actual}; expected consecutive ordinal {expected}"
    )]
    NonCanonicalArtifactOrdinal {
        index: usize,
        expected: u32,
        actual: u32,
    },
    #[error("extraction manifest contains duplicate object address {0:?}")]
    DuplicateArtifactAddress(ObjectAddress),
    #[error("manifest paths {first:?} and {second:?} cannot coexist on portable filesystems")]
    DuplicateArtifactPath { first: String, second: String },
    #[error("artifact status {status:?} is inconsistent with its length and digest evidence")]
    InvalidArtifactEvidence { status: ExtractionArtifactStatus },
    #[error("failed artifact {ordinal} has no stable diagnostic")]
    FailedArtifactWithoutDiagnostic { ordinal: u32 },
    #[error("manifest artifact {ordinal} contains a diagnostic for another object")]
    InvalidDiagnosticAddress { ordinal: u32 },
    #[error("serialized extraction report counts do not match its manifest")]
    ReportCountMismatch,
    #[error(transparent)]
    Model(#[from] ExtractionModelError),
    #[error(transparent)]
    Canonical(#[from] ExtractionCanonicalError),
}
